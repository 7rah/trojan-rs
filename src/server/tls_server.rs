use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use mio::net::TcpListener;
use mio::{Event, Poll, Token};
use rustls::{ServerConfig, ServerSession};

use crate::config::Opts;
use crate::server::connection::Connection;
use crate::server::{CHANNEL_CNT, CHANNEL_PROXY, MAX_INDEX, MIN_INDEX};
use crate::sys;
use crate::tls_conn::{ConnStatus, TlsConn};

pub struct TlsServer {
    listener: TcpListener,
    config: Arc<ServerConfig>,
    next_id: usize,
    conns: HashMap<usize, Connection>,
}

pub trait Backend {
    fn ready(&mut self, event: &Event, opts: &mut Opts, conn: &mut TlsConn<ServerSession>);
    fn dispatch(&mut self, data: &[u8], opts: &mut Opts);
    fn reregister(&mut self, poll: &Poll, readable: bool);
    fn check_close(&mut self, poll: &Poll);
    fn closing(&self) -> bool {
        if let ConnStatus::Closing = self.status() {
            true
        } else {
            false
        }
    }
    fn closed(&self) -> bool {
        if let ConnStatus::Closed = self.status() {
            true
        } else {
            false
        }
    }
    fn timeout(&self, t1: Instant, t2: Instant) -> bool {
        t2 - t1 > self.get_timeout()
    }
    fn get_timeout(&self) -> Duration;
    fn status(&self) -> ConnStatus;
    fn shutdown(&mut self, poll: &Poll);
    fn writable(&self) -> bool;
}

impl TlsServer {
    pub fn new(listener: TcpListener, config: Arc<ServerConfig>) -> TlsServer {
        TlsServer {
            listener,
            config,
            next_id: 2,
            conns: HashMap::new(),
        }
    }

    pub fn accept(&mut self, poll: &Poll, opts: &Opts) {
        loop {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    log::debug!(
                        "get new connection, token:{}, address:{}",
                        self.next_id,
                        addr
                    );
                    if let Err(err) = sys::set_mark(&stream, opts.marker) {
                        log::error!("set mark failed:{}", err);
                        continue;
                    } else if let Err(err) = stream.set_nodelay(true) {
                        log::error!("set nodelay failed:{}", err);
                        continue;
                    }
                    let session = ServerSession::new(&self.config);
                    let index = self.next_index();
                    let mut conn = Connection::new(
                        index,
                        TlsConn::new(
                            index,
                            Token(index * CHANNEL_CNT + CHANNEL_PROXY),
                            session,
                            stream,
                        ),
                    );
                    if conn.setup(poll, opts) {
                        self.conns.insert(index, conn);
                    } else {
                        conn.close_now(poll);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    log::debug!("no more connection to be accepted");
                    break;
                }
                Err(err) => {
                    log::error!("accept failed with error:{}, exit now", err);
                    panic!(err)
                }
            }
        }
    }

    fn next_index(&mut self) -> usize {
        let index = self.next_id;
        self.next_id += 1;
        if self.next_id > MAX_INDEX {
            self.next_id = MIN_INDEX;
        }
        index
    }

    fn token2index(&mut self, token: Token) -> usize {
        token.0 / CHANNEL_CNT
    }

    pub fn do_conn_event(&mut self, poll: &Poll, event: &Event, opts: &mut Opts) {
        let index = self.token2index(event.token());
        if self.conns.contains_key(&index) {
            let conn = self.conns.get_mut(&index).unwrap();
            conn.ready(poll, event, opts);
            if conn.destroyed() {
                self.conns.remove(&index);
                log::debug!("connection:{} closed, remove from pool", index);
            }
        } else {
            log::error!("connection:{} not found", index);
        }
    }

    pub fn check_timeout(&mut self, check_active_time: Instant, poll: &Poll) {
        let mut list = Vec::new();
        for (index, conn) in &mut self.conns {
            if conn.timeout(check_active_time) {
                list.push(*index);
                log::warn!("connection:{} timeout, close now", index);
                conn.close_now(poll)
            }
        }

        for index in list {
            self.conns.remove(&index);
        }
    }
}
