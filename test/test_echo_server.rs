use {TryRead, TryWrite};
use mio::{Events, Poll, PollOpt, Ready, Token};
use mio_uds_windows::{UnixListener, UnixStream};
use bytes::{Buf, ByteBuf, MutByteBuf, SliceBuf};
use slab::Slab;
use std::io;
use tempdir::TempDir;

const SERVER: Token = Token(10_000_000);
const CLIENT: Token = Token(10_000_001);

struct EchoConn {
    sock: UnixStream,
    buf: Option<ByteBuf>,
    mut_buf: Option<MutByteBuf>,
    token: Option<Token>,
    interest: Ready
}

impl EchoConn {
    fn new(sock: UnixStream) -> EchoConn {
        EchoConn {
            sock: sock,
            buf: None,
            mut_buf: Some(ByteBuf::mut_with_capacity(2048)),
            token: None,
            interest: Ready::empty(),
        }
    }

    fn writable(&mut self, poll: &mut Poll) -> io::Result<()> {
        let mut buf = self.buf.take().unwrap();

        match self.sock.try_write_buf(&mut buf) {
            Ok(None) => {
                debug!("client flushing buf; WOULDBLOCK");

                self.buf = Some(buf);
                self.interest.insert(Ready::writable());
            }
            Ok(Some(r)) => {
                debug!("CONN : we wrote {} bytes!", r);

                self.mut_buf = Some(buf.flip());

                self.interest.insert(Ready::readable());
                self.interest.remove(Ready::writable());
            }
            Err(e) => debug!("not implemented; client err={:?}", e),
        }

        assert!(self.interest.is_readable() || self.interest.is_writable(), "actual={:?}", self.interest);
        poll.reregister(&self.sock, self.token.unwrap(), self.interest,
                              PollOpt::edge() | PollOpt::oneshot())
    }

    fn readable(&mut self, poll: &mut Poll) -> io::Result<()> {
        let mut buf = self.mut_buf.take().unwrap();

        match self.sock.try_read_buf(&mut buf) {
            Ok(None) => {
                debug!("CONN : spurious read wakeup");
                self.mut_buf = Some(buf);
            }
            Ok(Some(r)) => {
                debug!("CONN : we read {} bytes!", r);

                // prepare to provide this to writable
                self.buf = Some(buf.flip());

                self.interest.remove(Ready::readable());
                self.interest.insert(Ready::writable());
            }
            Err(e) => {
                debug!("not implemented; client err={:?}", e);
                self.interest.remove(Ready::readable());
            }

        };

        assert!(self.interest.is_readable() || self.interest.is_writable(), "actual={:?}", self.interest);
        poll.reregister(&self.sock, self.token.unwrap(), self.interest,
                              PollOpt::edge())
    }
}

struct EchoServer {
    sock: UnixListener,
    conns: Slab<EchoConn>
}

impl EchoServer {
    fn accept(&mut self, poll: &mut Poll) -> io::Result<()> {
        debug!("server accepting socket");

        let sock = self.sock.accept().unwrap().unwrap().0;
        let conn = EchoConn::new(sock,);
        let tok = self.conns.insert(conn);

        // Register the connection
        self.conns[tok].token = Some(Token(tok));
        poll.register(&self.conns[tok].sock, Token(tok), Ready::readable(),
                                PollOpt::edge() | PollOpt::oneshot())
            .ok().expect("could not register socket with event loop");

        Ok(())
    }

    fn conn_readable(&mut self, poll: &mut Poll,
                     tok: Token) -> io::Result<()> {
        debug!("server conn readable; tok={:?}", tok);
        self.conn(tok).readable(poll)
    }

    fn conn_writable(&mut self, poll: &mut Poll,
                     tok: Token) -> io::Result<()> {
        debug!("server conn writable; tok={:?}", tok);
        self.conn(tok).writable(poll)
    }

    fn conn<'a>(&'a mut self, tok: Token) -> &'a mut EchoConn {
        &mut self.conns[tok.into()]
    }
}

struct EchoClient {
    sock: UnixStream,
    msgs: Vec<&'static str>,
    tx: SliceBuf<'static>,
    rx: SliceBuf<'static>,
    mut_buf: Option<MutByteBuf>,
    token: Token,
    interest: Ready,
    shutdown: bool,
}


// Sends a message and expects to receive the same exact message, one at a time
impl EchoClient {
    fn new(sock: UnixStream, tok: Token,  mut msgs: Vec<&'static str>) -> EchoClient {
        let curr = msgs.remove(0);

        EchoClient {
            sock: sock,
            msgs: msgs,
            tx: SliceBuf::wrap(curr.as_bytes()),
            rx: SliceBuf::wrap(curr.as_bytes()),
            mut_buf: Some(ByteBuf::mut_with_capacity(2048)),
            token: tok,
            interest: Ready::empty(),
            shutdown: false,
        }
    }

    fn readable(&mut self, poll: &mut Poll) -> io::Result<()> {
        debug!("client socket readable");

        let mut buf = self.mut_buf.take().unwrap();

        match self.sock.try_read_buf(&mut buf) {
            Ok(None) => {
                debug!("CLIENT : spurious read wakeup");
                self.mut_buf = Some(buf);
            }
            Ok(Some(r)) => {
                debug!("CLIENT : We read {} bytes!", r);

                // prepare for reading
                let mut buf = buf.flip();

                while buf.has_remaining() {
                    let actual = buf.read_byte().unwrap();
                    let expect = self.rx.read_byte().unwrap();

                    assert!(actual == expect, "actual={}; expect={}", actual, expect);
                }

                self.mut_buf = Some(buf.flip());

                self.interest.remove(Ready::readable());

                if !self.rx.has_remaining() {
                    self.next_msg(poll).unwrap();
                }
            }
            Err(e) => {
                panic!("not implemented; client err={:?}", e);
            }
        };

        if !self.interest.is_empty() {
            assert!(self.interest.is_readable() || self.interest.is_writable(), "actual={:?}", self.interest);
            poll.reregister(&self.sock, self.token, self.interest,
                                       PollOpt::edge() | PollOpt::oneshot())?;
        }

        Ok(())
    }

    fn writable(&mut self, poll: &mut Poll) -> io::Result<()> {
        debug!("client socket writable");

        match self.sock.try_write_buf(&mut self.tx) {
            Ok(None) => {
                debug!("client flushing buf; WOULDBLOCK");
                self.interest.insert(Ready::writable());
            }
            Ok(Some(r)) => {
                debug!("CLIENT : we wrote {} bytes!", r);
                self.interest.insert(Ready::readable());
                self.interest.remove(Ready::writable());
            }
            Err(e) => debug!("not implemented; client err={:?}", e)
        }

        if self.interest.is_readable() || self.interest.is_writable() {
            try!(poll.reregister(&self.sock, self.token, self.interest,
                                  PollOpt::edge() | PollOpt::oneshot()));
        }

        Ok(())
    }

    fn next_msg(&mut self, poll: &mut Poll) -> io::Result<()> {
        if self.msgs.is_empty() {
            self.shutdown = true;
            return Ok(());
        }

        let curr = self.msgs.remove(0);

        debug!("client prepping next message");
        self.tx = SliceBuf::wrap(curr.as_bytes());
        self.rx = SliceBuf::wrap(curr.as_bytes());

        self.interest.insert(Ready::writable());
        poll.reregister(&self.sock, self.token, self.interest,
                              PollOpt::edge() | PollOpt::oneshot())
    }
}

struct Echo {
    server: EchoServer,
    client: EchoClient,
}

impl Echo {
    fn new(srv: UnixListener, client: UnixStream, msgs: Vec<&'static str>) -> Echo {
        Echo {
            server: EchoServer {
                sock: srv,
                conns: Slab::with_capacity(128)
            },
            client: EchoClient::new(client, CLIENT, msgs)
        }
    }
}

#[test]
pub fn test_echo_server() {
    debug!("Starting TEST_ECHO_SERVER");
    let mut poll = Poll::new().unwrap();
    let dir = TempDir::new("uds").unwrap();

    let srv = UnixListener::bind(dir.path().join("foo")).unwrap();
    let addr = srv.local_addr().unwrap();

    info!("listen for connections");
    poll.register(&srv, SERVER, Ready::readable(),
                            PollOpt::edge() | PollOpt::oneshot()).unwrap();

    let sock = UnixStream::connect(&addr.as_pathname().unwrap()).unwrap();

    // Connect to the server
    poll.register(&sock, CLIENT, Ready::writable(),
                        PollOpt::edge() | PollOpt::oneshot()).unwrap();
    // == Create storage for events
    let mut events = Events::with_capacity(1024);

    let mut handler = Echo::new(srv, sock, vec!["foo", "bar"]);

    // Start the event loop
    while !handler.client.shutdown {
        poll.poll(&mut events, None).unwrap();

        for event in &events {
            debug!("ready {:?} {:?}", event.token(), event.readiness());
            if event.readiness().is_readable() {
                match event.token() {
                    SERVER => handler.server.accept(&mut poll).unwrap(),
                    CLIENT => handler.client.readable(&mut poll).unwrap(),
                    i => handler.server.conn_readable(&mut poll, i).unwrap()
                }
            }

            if event.readiness().is_writable() {
                match event.token() {
                    SERVER => panic!("received writable for token 0"),
                    CLIENT => handler.client.writable(&mut poll).unwrap(),
                    i => handler.server.conn_writable(&mut poll, i).unwrap()
                };
            }
        }
    }
}
