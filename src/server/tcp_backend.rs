use std::net::Shutdown;
use std::time::Duration;

use bytes::BytesMut;
use mio::net::TcpStream;
use mio::{Event, Poll, PollOpt, Ready, Token};
use rustls::ServerSession;

use crate::config::Opts;
use crate::proto::{MAX_BUFFER_SIZE, MAX_PACKET_SIZE};
use crate::server::tls_server::Backend;
use crate::tcp_util;
use crate::tls_conn::{ConnStatus, TlsConn};

pub struct TcpBackend {
    conn: TcpStream,
    status: ConnStatus,
    readiness: Ready,
    index: usize,
    token: Token,
    timeout: Duration,
    send_buffer: BytesMut,
    recv_buffer: Vec<u8>,
}

impl TcpBackend {
    pub fn new(conn: TcpStream, index: usize, token: Token, timeout: Duration) -> TcpBackend {
        TcpBackend {
            conn,
            timeout,
            status: ConnStatus::Established,
            readiness: Ready::readable(),
            send_buffer: BytesMut::new(),
            recv_buffer: vec![0u8; MAX_PACKET_SIZE],
            index,
            token,
        }
    }
    fn do_read(&mut self, conn: &mut TlsConn<ServerSession>) {
        if !tcp_util::tcp_read(self.index, &self.conn, &mut self.recv_buffer, conn) {
            self.status = ConnStatus::Closing;
        }

        conn.do_send();
    }

    fn do_send(&mut self, data: &[u8]) {
        if !tcp_util::tcp_send(self.index, &self.conn, &mut self.send_buffer, data) {
            self.status = ConnStatus::Closing;
            return;
        }

        if let ConnStatus::Shutdown = self.status {
            if self.send_buffer.is_empty() {
                log::debug!("connection:{} is closing for no data to send", self.index);
                self.status = ConnStatus::Closing;
            }
        }
    }

    fn setup(&mut self, poll: &Poll) {
        if let Err(err) = poll.reregister(&self.conn, self.token, self.readiness, PollOpt::edge()) {
            log::error!(
                "connection:{} reregister tcp target failed:{}",
                self.index,
                err
            );
            self.status = ConnStatus::Closing;
        }
    }
}

impl Backend for TcpBackend {
    fn ready(&mut self, event: &Event, opts: &mut Opts, conn: &mut TlsConn<ServerSession>) {
        if event.readiness().is_readable() {
            self.do_read(conn);
        }
        if event.readiness().is_writable() {
            self.dispatch(&[], opts);
        }
    }

    fn dispatch(&mut self, buffer: &[u8], _: &mut Opts) {
        // send immediately first
        if self.send_buffer.is_empty() {
            self.do_send(buffer);
        } else {
            self.send_buffer.extend_from_slice(buffer);
            let buffer = self.send_buffer.split();
            self.do_send(buffer.as_ref());
        }
    }

    fn reregister(&mut self, poll: &Poll, readable: bool) {
        match self.status {
            ConnStatus::Closing => {
                let _ = poll.deregister(&self.conn);
            }
            ConnStatus::Closed => {}
            _ => {
                let mut changed = false;
                if !self.send_buffer.is_empty() && !self.readiness.is_writable() {
                    self.readiness.insert(Ready::writable());
                    changed = true;
                    log::debug!("connection:{} add writable to tcp target", self.index);
                }
                if self.send_buffer.is_empty() && self.readiness.is_writable() {
                    self.readiness.remove(Ready::writable());
                    changed = true;
                    log::debug!("connection:{} remove writable from tcp target", self.index);
                }
                if readable && !self.readiness.is_readable() {
                    self.readiness.insert(Ready::readable());
                    log::debug!("connection:{} add readable to tcp target", self.index);
                    changed = true;
                }
                if !readable && self.readiness.is_readable() {
                    self.readiness.remove(Ready::readable());
                    log::debug!("connection:{} remove readable from tcp target", self.index);
                    changed = true;
                }

                if changed {
                    self.setup(poll);
                }
            }
        }
    }

    fn check_close(&mut self, poll: &Poll) {
        if let ConnStatus::Closing = self.status {
            let _ = poll.deregister(&self.conn);
            let _ = self.conn.shutdown(Shutdown::Both);
            self.status = ConnStatus::Closed;
        }
    }

    fn get_timeout(&self) -> Duration {
        self.timeout
    }

    fn status(&self) -> ConnStatus {
        self.status
    }

    fn shutdown(&mut self, poll: &Poll) {
        if self.send_buffer.is_empty() {
            self.status = ConnStatus::Closing;
            self.check_close(poll);
            return;
        }

        self.readiness = Ready::writable();
        self.status = ConnStatus::Shutdown;
        self.setup(poll);
        self.check_close(poll);
    }

    fn writable(&self) -> bool {
        self.send_buffer.len() < MAX_BUFFER_SIZE
    }
}
