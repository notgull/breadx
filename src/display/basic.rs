// MIT/Apache2 License

use super::{
    input, name::NameConnection, Connection, Display, DisplayBase, PendingReply, PendingRequest,
    RequestInfo,
};
use crate::{
    auth_info::AuthInfo, auto::xproto::Setup, error::BreadError, event::Event, util::difference,
    XidGenerator, XID,
};
use alloc::collections::VecDeque;
use core::num::NonZeroU32;
use hashbrown::HashMap;

#[cfg(feature = "async")]
use super::{AsyncConnection, AsyncDisplay};

/// An implementor of `Display` and `AsyncDisplay` that requires &mut access in order to use.
pub struct StandardDisplay<Conn> {
    /// The connection to the server. It is in an `Option`, so that way if it is `None` we know
    /// the connection has been poisoned.
    connection: Option<Conn>,

    /// The setup received from the server.
    setup: Setup,

    /// XID generator.
    xid: XidGenerator,

    /// The index of the screen to be used by default.
    default_screen: usize,

    /// Queue for events; more recent events are at the front.
    event_queue: VecDeque<Event>,
    /// Map associating request numbers to pending requests, that have not been replied to by
    /// the server yet.
    pending_requests: HashMap<u16, PendingRequest>,
    /// Map associating request numbers to requests that have error'd out. This map is unlikely
    /// to ever hold many entries; it might be worth reconsidering its type.
    pending_errors: HashMap<u16, BreadError>,
    /// Map associating request numbers with replies sent by the server.
    pending_replies: HashMap<u16, PendingReply>,

    // special events queue
    special_event_queues: HashMap<XID, VecDeque<Event>>,

    request_number: u64,

    // store the interned atoms
    wm_protocols_atom: Option<NonZeroU32>,

    // tell whether or not we care about the output of zero-sized replies
    checked: bool,

    // hashmap linking extension names to major opcodes
    // we use byte arrays instead of static string pointers
    // here because cache locality leads to an overall speedup (todo: verify)
    extensions: HashMap<[u8; EXT_KEY_SIZE], u8>,

    // internal buffer for polling for waiting
    #[cfg(feature = "async")]
    wait_buffer: Option<WaitBuffer>,

    // internal buffer for sending a request
    #[cfg(feature = "async")]
    send_buffer: SendBuffer,

    /// List of requests we need to consider the GLX workaround for. This simplifies
    /// async operations.
    #[cfg(feature = "async")]
    workarounders: Vec<u16>,
}

impl<Conn> StandardDisplay<Conn> {
    #[inline]
    fn from_connection_internal(connection: Conn, default_screen: usize) -> Self {
        Self {
            connection: Some(connection),
            setup: Default::default(),
            xid: Default::default(),
            default_screen,
            event_queue: VecDeque::with_capacity(8),
            // setting this to 1 because breadglx with DRI3 will always append one entry to this map,
            // and expanding this map is considered to be a cold operation
            special_event_queues: HashMap::with_capacity(1),
            pending_requests: HashMap::with_capacity(4),
            pending_replies: HashMap::with_capacity(4),
            pending_errors: HashMap::with_capacity(4),
            request_number: 1,
            wm_protocols_atom: None,
            checked: cfg!(debug_assertions),
            //            context: HashMap::new(),
            extensions: HashMap::with_capacity(8),
            #[cfg(feature = "async")]
            wait_buffer: None,
            #[cfg(feature = "async")]
            send_buffer: Default::default(),
            #[cfg(feature = "async")]
            workarounders: vec![],
        }
    }
}

impl<Conn: Connection> StandardDisplay<Conn> {
    #[inline]
    pub fn from_connection(
        connection: Conn,
        default_screen: usize,
        auth_info: Option<AuthInfo>,
    ) -> crate::Result<Self> {
        let mut this = Self::from_connection_internal(connection, default_screen);
        let (setup, xid) = self.connection.as_mut().unwrap().establish()?;
        this.setup = setup;
        this.xid = xid;
        Ok(this)
    }
}

#[cfg(feature = "async")]
impl<Conn: AsyncConnection> StandardDisplay<Conn> {
    #[inline]
    pub async fn from_connection_async(
        connection: Conn,
        default_screen: usize,
        auth_info: Option<AuthInfo>,
    ) -> crate::Result<Self> {
        let mut this = Self::from_connection_internal(connection, default_screen);
        let (setup, xid) = self.connection.as_mut().unwrap().establish().await?;
        this.setup = setup;
        this.xid = xid;
        Ok(this)
    }
}

impl<Conn> DisplayBase for StandardDisplay<Conn> {
    #[inline]
    fn setup(&self) -> &Setup {
        &self.setup
    }

    #[inline]
    fn default_screen(&self) -> usize {
        self.default_screen
    }

    #[inline]
    fn next_request_number(&mut self) -> u64 {
        let rn = self.request_number;
        self.request_number = self.request_number.wrapping_add(1);
        rn
    }

    #[inline]
    fn push_event(&mut self, event: Event) {
        self.event_queue.push_back(event)
    }

    #[inline]
    fn pop_event(&mut self) -> Option<Event> {
        self.event_queue.pop_front()
    }

    #[inline]
    fn generate_xid(&mut self) -> Option<XID> {
        self.xid.next()
    }

    #[inline]
    fn add_pending_request(&mut self, req_id: u16, pereq: PendingRequest) {
        #[cfg(feature = "async")]
        if matches!(pereq.flags.workaround, RequestWorkaround::GlxFbconfigBug) {
            self.workarounders.push(req_id);
        }
        self.pending_requests.insert(req_id, pereq);
    }

    #[inline]
    fn get_pending_request(&self, req_id: u16) -> Option<PendingRequest> {
        self.pending_requests.get(&req_id).cloned()
    }

    #[inline]
    fn remove_pending_request(&mut self, req_id: u16) -> Option<PendingRequest> {
        #[cfg(feature = "async")]
        self.workarounders.retain(|&r| r != req_id);
        self.pending_requests.remove(&req_id)
    }

    #[inline]
    fn add_pending_error(&mut self, req_id: u16, error: BreadError) {
        self.pending_errors.insert(req_id, error)
    }

    #[inline]
    fn check_for_pending_error(&mut self, req_id: u16) -> crate::Result<()> {
        match self.pending_errors.remove(&req_id) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    #[inline]
    fn add_pending_reply(&mut self, req_id: u16, reply: PendingReply) {
        self.pending_replies.insert(req_id, reply);
    }

    #[inline]
    fn take_pending_reply(&mut self, req_id: u16) -> Option<PendingReply> {
        self.pending_replies.remove(&req_id)
    }

    #[inline]
    fn create_special_event_queue(&mut self, xid: XID) {
        self.special_event_queues.insert(xid, VecDeque::new());
    }

    #[inline]
    fn push_special_event(&mut self, xid: XID, event: Event) -> Result<(), Event> {
        match self.special_event_queues.get_mut(&xid) {
            Some(queue) => {
                queue.push_back(event);
                Ok(())
            }
            None => Err(event),
        }
    }

    #[inline]
    fn pop_special_event(&mut self, xid: XID) -> Option<Event> {
        self.special_event_queues
            .get_mut(&xid)
            .and_then(|queue| queue.pop_front())
    }

    #[inline]
    fn delete_special_event_queue(&mut self, xid: XID) {
        self.special_event_queues.remove(&xid);
    }

    #[inline]
    fn checked(&self) -> bool {
        self.checked
    }

    #[inline]
    fn set_checked(&mut self, checked: bool) {
        self.checked = checked;
    }

    #[inline]
    fn get_extension_opcode(&mut self, key: &[u8; EXT_KEY_SIZE]) -> Option<u8> {
        self.extensions.get(key).copied()
    }

    #[inline]
    fn set_extension_opcode(&mut self, key: [u8; EXT_KEY_SIZE], opcode: u8) {
        self.extensions.insert(key, opcode)
    }

    #[inline]
    fn wm_protocols_atom(&self) -> Option<NonZeroU32> {
        self.wm_protocols_atom
    }

    #[inline]
    fn set_wm_protocols_atom(&mut self, a: NonZeroU32) {
        self.wm_protocols_atom = Some(a);
    }
}

impl<Connect: Connection> Display for StandardDisplay<Connect> {
    type Conn = Connect;

    #[inline]
    fn connection(&mut self) -> &mut Connect {
        self.connection.as_mut().expect("Connection was poisoned")
    }

    // locks mean nothing, &mut access is necessary to do anything
    #[inline]
    fn lock(&mut self) {}

    #[inline]
    fn unlock(&mut self) {}
}

#[cfg(feature = "async")]
impl<Connect: AsyncConnection + Unpin> AsyncDisplay for StandardDisplay<Connect> {
    type Conn = Connect;

    #[inline]
    fn connection(&mut self) -> &mut Connect {
        self.connection.as_mut().expect("Connection was poisoned")
    }

    #[inline]
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result> {
        let pr_ref = &mut self.pending_requests;
        let (bytes, fds) = match self
            .wait_buffer
            .get_or_insert_with(WaitBuffer::default)
            .poll_wait(self.connection.as_mut().unwrap(), &self.workarounders, cx)
        {
            Poll::Ready(res) => {
                self.wait_buffer.take();
                match res {
                    Ok(WaitBufferReturn { data, fds }) => (data, fds),
                    Err(e) => return Poll::Ready(Err(e)),
                }
            }
            Poll::Pending => return Poll::Pending,
        };

        Poll::Ready(input::process_bytes(self, bytes, fds))
    }

    #[inline]
    fn begin_send_request_raw(&mut self, req: RequestInfo) {
        self.send_buffer.fill_hole(req);
    }

    #[inline]
    fn poll_send_request_raw(&mut self, cx: &mut Context<'_>) -> Poll<crate::Result<u16>> {
        let mut send_buffer = mem::replace(&mut self.send_buffer, SendBuffer::OccupiedHole);
        let res = send_buffer.poll_send_request(self, cx);
        self.send_buffer = send_buffer;
        if res.is_ready() {
            self.send_buffer.dig_hole();
        }
        match res {
            Poll::Ready(Ok(pr)) => Poll::Ready(output::finish_request(self, pr)),
            res => res,
        }
    }
}

/// A variant of `BasicDisplay` that uses X11's default connection mechanisms to connect to the server. In
/// most cases, you should be using either this, or converting this type to a `CellDisplay` or `SyncDisplay`.
#[cfg(feature = "std")]
pub type DisplayConnection = BasicDisplay<NameConnection>;

#[cfg(all(feature = "std", feature = "async"))]
pub type AsyncDisplayConnection = BasicDisplay<AsyncNameConnection>;

#[cfg(feature = "std")]
impl DisplayConnection {
    /// Create a new connection to the X server, given an optional name and authorization information.
    #[inline]
    pub fn create(name: Option<Cow<'_, str>>, auth_info: Option<AuthInfo>) -> crate::Result<Self> {
        let (connection, screen) = NameConnection::connect_internal(name)?;
        Self::from_connection(connection, screen, auth_info)
    }
}

#[cfg(all(feature = "std", feature = "async"))]
impl AsyncDisplayConnection {
    /// Create a new connection to the X server, given an optional name and authorization information, async
    /// redox.
    #[cfg(feature = "async")]
    #[inline]
    pub async fn create_async(
        name: Option<Cow<'_, str>>,
        auth_info: Option<AuthInfo>,
    ) -> crate::Result<Self> {
        let (connection, screen) = name::AsyncNameConnection::connect_internal_async(name).await?;
        Self::from_connection_async(connection, screen, auth_info).await
    }
}
