use std::collections::VecDeque;
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, RawFd};
use std::{io, ptr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_uring::opcode::{self, types};
use io_uring::{squeue, concurrent, IoUring};
use lazy_static::lazy_static;
use slab::Slab;

lazy_static! {
    static ref TOKEN_QUEUE: Mutex<VecDeque<(usize, i32)>> = Mutex::new(VecDeque::new());
}

#[derive(Clone, Debug)]
enum Token {
    Accept,
    Poll {
        fd: RawFd,
    },
    Read {
        fd: RawFd,
        buf_index: usize,
    },
    Write {
        fd: RawFd,
        buf_index: usize,
        offset: usize,
        len: usize,
    },
}

pub struct AcceptCount {
    entry: squeue::Entry,
    count: usize,
}

impl AcceptCount {
    fn new(fd: RawFd, token: usize, count: usize) -> AcceptCount {
        AcceptCount {
            entry: opcode::Accept::new(types::Fd(fd), ptr::null_mut(), ptr::null_mut())
                .build()
                .user_data(token as _),
            count,
        }
    }

    pub fn push_to(&mut self, ring: &concurrent::IoUring) {
        while self.count > 0 {
            unsafe {
                match ring.submission().push(self.entry.clone()) {
                    Ok(_) => self.count -= 1,
                    Err(_) => break,
                }
            }
            ring.submit();
        }
    }
}

fn main() -> anyhow::Result<()> {
    let ring = IoUring::new(256)?.concurrent();
    ring.start_enter_syscall_thread();
    let listener = TcpListener::bind(("127.0.0.1", 3456))?;

    let mut bufpool = Vec::with_capacity(64);
    let mut buf_alloc = Slab::with_capacity(64);
    let mut token_alloc = Slab::with_capacity(64);

    println!("tcp_echo_concurrent2");
    println!("listen {}", listener.local_addr()?);

    let mut accept = AcceptCount::new(listener.as_raw_fd(), token_alloc.insert(Token::Accept), 3);

    loop {
        accept.push_to(&ring);

        while let Some(cqe) = ring.completion().pop() {
            let ret = cqe.result();
            let token_index = cqe.user_data() as usize;

            if ret < 0 {
                eprintln!(
                    "token {:?} error: {:?}",
                    token_alloc.get(token_index),
                    io::Error::from_raw_os_error(-ret)
                );
                continue;
            }
            
            let mut queue = TOKEN_QUEUE.lock().unwrap();
            queue.push_back((token_index, ret));
        }

        let mut queue = TOKEN_QUEUE.lock().unwrap();
        while !queue.is_empty() {
            let (token_index, ret) = queue.pop_front().unwrap();
            let token = &mut token_alloc[token_index];

            match token.clone() {
                Token::Accept => {
                    println!("accept");

                    accept.count += 1;

                    let fd = ret;
                    let poll_token = token_alloc.insert(Token::Poll { fd });

                    let poll_e = opcode::PollAdd::new(types::Fd(fd), libc::POLLIN as _)
                        .build()
                        .user_data(poll_token as _);

                    unsafe {
                        if let Err(entry) = ring.submission().push(poll_e) {
                            println!("push error!")
                        }
                    }
                    ring.submit();
                }
                Token::Poll { fd } => {
                    let (buf_index, buf) = match bufpool.pop() {
                        Some(buf_index) => (buf_index, &mut buf_alloc[buf_index]),
                        None => {
                            let buf = vec![0u8; 2048].into_boxed_slice();
                            let buf_entry = buf_alloc.vacant_entry();
                            let buf_index = buf_entry.key();
                            (buf_index, buf_entry.insert(buf))
                        }
                    };

                    *token = Token::Read { fd, buf_index };

                    let read_e = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), buf.len() as _)
                        .build()
                        .user_data(token_index as _);

                    unsafe {
                        if let Err(entry) = ring.submission().push(read_e) {
                            println!("push error!")
                        }
                    }
                    ring.submit();
                }
                Token::Read { fd, buf_index } => {
                    if ret == 0 {
                        bufpool.push(buf_index);
                        token_alloc.remove(token_index);

                        println!("shutdown");

                        unsafe {
                            libc::close(fd);
                        }
                    } else {
                        let len = ret as usize;
                        let buf = &buf_alloc[buf_index];

                        *token = Token::Write {
                            fd,
                            buf_index,
                            len,
                            offset: 0,
                        };

                        let write_e = opcode::Write::new(types::Fd(fd), buf.as_ptr(), len as _)
                            .build()
                            .user_data(token_index as _);

                        unsafe {
                            if let Err(entry) = ring.submission().push(write_e) {
                                println!("push error!")
                            }
                        }
                    }
                    ring.submit();
                }
                Token::Write {
                    fd,
                    buf_index,
                    offset,
                    len,
                } => {
                    let write_len = ret as usize;

                    let entry = if offset + write_len >= len {
                        bufpool.push(buf_index);

                        *token = Token::Poll { fd };

                        opcode::PollAdd::new(types::Fd(fd), libc::POLLIN as _)
                            .build()
                            .user_data(token_index as _)
                    } else {
                        let offset = offset + write_len;
                        let len = len - offset;

                        let buf = &buf_alloc[buf_index][offset..];

                        *token = Token::Write {
                            fd,
                            buf_index,
                            offset,
                            len,
                        };

                        opcode::Write::new(types::Fd(fd), buf.as_ptr(), len as _)
                            .build()
                            .user_data(token_index as _)
                    };

                    unsafe {
                        if let Err(entry) = ring.submission().push(entry) {
                            println!("push error!")
                        }
                    }
                    ring.submit();
                }
            }
        }
    }
}
