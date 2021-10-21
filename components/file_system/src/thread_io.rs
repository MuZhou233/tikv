// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use crate::IOBytes;
use crate::IOType;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use dashmap::DashMap;
use nix::unistd::getpid;
use nix::unistd::gettid;
use nix::unistd::Pid;
use strum::EnumCount;

lazy_static! {
    static ref THREAD_IO_BYTES_VEC: DashMap<ThreadID, BytesBuffer> = DashMap::new();
}

#[derive(PartialEq, Eq, Hash, Clone)]
struct ThreadID {
    pid: Pid,
    tid: Pid,
}

struct BytesBuffer {
    current_io_type: IOType,
    total_io_bytes: [IOBytes; IOType::COUNT],
    last_io_bytes: [IOBytes; IOType::COUNT],
}

pub fn init_thread_io() {
    std::thread::spawn(|| {
        loop {
            THREAD_IO_BYTES_VEC.alter_all(|k, mut v| {
                let io_bytes = fetch_exact_thread_io_bytes(k);
                v.total_io_bytes[v.current_io_type as usize] +=
                    io_bytes - v.last_io_bytes[v.current_io_type as usize];
                v.last_io_bytes[v.current_io_type as usize] = io_bytes;
                v
            });
            std::thread::sleep(Duration::from_millis(10));
        }
    });
}

pub fn set_io_type(new_io_type: IOType) {
    let id = ThreadID::current();

    if !THREAD_IO_BYTES_VEC.contains_key(&id) {
        THREAD_IO_BYTES_VEC.insert(
            id.clone(),
            BytesBuffer {
                current_io_type: IOType::Other,
                total_io_bytes: [IOBytes::default(); IOType::COUNT],
                last_io_bytes: [IOBytes::default(); IOType::COUNT],
            },
        );
    }

    if let Some(mut buffer) = THREAD_IO_BYTES_VEC.get_mut(&id) {
        let io_bytes = fetch_exact_thread_io_bytes(&id);
        let old_io_type = buffer.current_io_type;
        let last_io_bytes = buffer.last_io_bytes[old_io_type as usize];
        buffer.total_io_bytes[old_io_type as usize] += io_bytes - last_io_bytes;

        buffer.current_io_type = new_io_type;
        buffer.last_io_bytes[new_io_type as usize] = io_bytes;
    }
}

pub fn get_io_type() -> IOType {
    if let Some(buffer) = THREAD_IO_BYTES_VEC.get(&ThreadID::current()) {
        buffer.current_io_type
    } else {
        IOType::Other
    }
}

pub(crate) fn fetch_buffered_thread_io_bytes(io_type: IOType) -> IOBytes {
    if let Some(buffer) = THREAD_IO_BYTES_VEC.get(&ThreadID::current()) {
        buffer.total_io_bytes[io_type as usize]
    } else {
        IOBytes::default()
    }
}

pub(crate) fn fetch_thread_io_bytes(_io_type: IOType) -> IOBytes {
    fetch_exact_thread_io_bytes(&ThreadID::current())
}

fn fetch_exact_thread_io_bytes(id: &ThreadID) -> IOBytes {
    let io_file_path = PathBuf::from("/proc")
        .join(format!("{}", id.pid))
        .join("task")
        .join(format!("{}", id.tid))
        .join("io");

    if let Ok(io_file) = File::open(io_file_path) {
        return IOBytes::from_io_file(io_file);
    }

    IOBytes::default()
}

impl ThreadID {
    fn current() -> ThreadID {
        ThreadID {
            pid: getpid(),
            tid: gettid(),
        }
    }
}

impl IOBytes {
    fn from_io_file<R: std::io::Read>(r: R) -> IOBytes {
        let reader = BufReader::new(r);
        let mut io_bytes = IOBytes::default();

        for line in reader.lines().flatten() {
            if line.is_empty() || !line.contains(' ') {
                continue;
            }
            let mut s = line.split_whitespace();

            if let (Some(field), Some(value)) = (s.next(), s.next()) {
                if let Ok(value) = u64::from_str(value) {
                    match &field[..field.len() - 1] {
                        "read_bytes" => io_bytes.read = value,
                        "write_bytes" => io_bytes.write = value,
                        _ => continue,
                    }
                }
            }
        }

        io_bytes
    }
}

#[cfg(test)]
mod tests {
    use libc::O_DIRECT;
    use maligned::{AsBytes, AsBytesMut, A512};
    use std::{
        fs::OpenOptions,
        io::{Read, Write},
        os::unix::prelude::OpenOptionsExt,
    };
    use tempfile::{tempdir, tempdir_in};

    use super::*;

    #[test]
    fn test_read_bytes() {
        let tmp = tempdir_in("/var/tmp").unwrap_or_else(|_| tempdir().unwrap());
        let file_path = tmp.path().join("test_read_bytes.txt");
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .custom_flags(O_DIRECT)
                .open(&file_path)
                .unwrap();
            let w = vec![A512::default(); 10];
            f.write_all(w.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }
        let mut f = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECT)
            .open(&file_path)
            .unwrap();
        let mut w = vec![A512::default(); 1];
        let origin_io_bytes = fetch_thread_io_bytes(IOType::Other);

        for i in 1..=10 {
            f.read_exact(w.as_bytes_mut()).unwrap();

            let io_bytes = fetch_thread_io_bytes(IOType::Other);

            assert_eq!(i * 512 + origin_io_bytes.read, io_bytes.read);
        }
    }

    #[test]
    fn test_write_bytes() {
        let tmp = tempdir_in("/var/tmp").unwrap_or_else(|_| tempdir().unwrap());
        let file_path = tmp.path().join("test_write_bytes.txt");
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .custom_flags(O_DIRECT)
            .open(&file_path)
            .unwrap();

        let w = vec![A512::default(); 1];

        let origin_io_bytes = fetch_thread_io_bytes(IOType::Other);
        for i in 1..=10 {
            f.write_all(w.as_bytes()).unwrap();
            f.sync_all().unwrap();

            let io_bytes = fetch_thread_io_bytes(IOType::Other);

            assert_eq!(i * 512 + origin_io_bytes.write, io_bytes.write);
        }
    }

    #[bench]
    fn bench_fetch_thread_io_bytes(b: &mut test::Bencher) {
        b.iter(|| fetch_thread_io_bytes(IOType::Other));
    }
}
