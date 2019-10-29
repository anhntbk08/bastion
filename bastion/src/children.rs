use crate::broadcast::{BastionMessage, Broadcast, Parent, Sender};
use crate::context::{BastionContext, BastionId, ContextState};
use crate::proc::Proc;
use crate::supervisor::SupervisorRef;
use futures::future::CatchUnwind;
use futures::pending;
use futures::poll;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use fxhash::FxHashMap;
use qutex::Qutex;
use std::any::Any;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::iter::FromIterator;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

pub trait Shell: Send + Sync + Any + 'static {}
impl<T> Shell for T where T: Send + Sync + Any + 'static {}

pub trait Message: Shell + Debug {}
impl<T> Message for T where T: Shell + Debug {}

#[derive(Debug)]
pub struct Msg(MsgInner);

#[derive(Debug)]
enum MsgInner {
    Shared(Arc<dyn Any + Send + Sync + 'static>),
    Owned(Box<dyn Any + Send + Sync + 'static>),
}

impl Msg {
    pub(crate) fn shared<M: Message>(msg: M) -> Self {
        let inner = MsgInner::Shared(Arc::new(msg));
        Msg(inner)
    }

    pub(crate) fn owned<M: Message>(msg: M) -> Self {
        let inner = MsgInner::Owned(Box::new(msg));
        Msg(inner)
    }

    pub fn is_broadcast(&self) -> bool {
        if let MsgInner::Shared(_) = self.0 {
            true
        } else {
            false
        }
    }

    pub fn downcast<M: Any>(self) -> Result<M, Self> {
        if let MsgInner::Owned(msg) = self.0 {
            if msg.is::<M>() {
                let msg: Box<dyn Any + 'static> = msg;
                Ok(*msg.downcast().unwrap())
            } else {
                let inner = MsgInner::Owned(msg);
                Err(Msg(inner))
            }
        } else {
            Err(self)
        }
    }

    pub fn downcast_ref<M>(&self) -> Option<Arc<M>>
    where
        M: Any + Send + Sync + 'static,
    {
        if let MsgInner::Shared(msg) = &self.0 {
            if msg.is::<M>() {
                return Some(msg.clone().downcast::<M>().unwrap());
            }
        }

        None
    }

    pub(crate) fn try_clone(&self) -> Option<Self> {
        if let MsgInner::Shared(msg) = &self.0 {
            let inner = MsgInner::Shared(msg.clone());
            Some(Msg(inner))
        } else {
            None
        }
    }

    pub(crate) fn try_unwrap<M>(self) -> Result<M, Self>
    where
        M: Any + Send + Sync + 'static,
    {
        if let MsgInner::Shared(msg) = self.0 {
            match msg.downcast() {
                Ok(msg) => {
                    match Arc::try_unwrap(msg) {
                        Ok(msg) => Ok(msg),
                        Err(msg) => {
                            let inner = MsgInner::Shared(msg);
                            Err(Msg(inner))
                        }
                    }
                }
                Err(msg) => {
                    let inner = MsgInner::Shared(msg);
                    Err(Msg(inner))
                }
            }
        } else {
            self.downcast()
        }
    }
}

pub trait Closure: Fn(BastionContext) -> Fut + Shell {}
impl<T> Closure for T where T: Fn(BastionContext) -> Fut + Shell {}

// TODO: Ok(T) & Err(E)
type FutInner = Pin<Box<dyn Future<Output = Result<(), ()>> + Send>>;
type Exec = CatchUnwind<AssertUnwindSafe<FutInner>>;

pub struct Fut(FutInner);

impl<T> From<T> for Fut
where
    T: Future<Output = Result<(), ()>> + Send + 'static,
{
    fn from(fut: T) -> Fut {
        Fut(Box::pin(fut))
    }
}

pub(crate) struct Children {
    bcast: Broadcast,
    supervisor: SupervisorRef,
    // The currently launched elements of the group.
    launched: FxHashMap<BastionId, (Sender, Proc<()>)>,
    // The closure returning the future that will be executed
    // by every element of the group.
    init: Box<dyn Closure>,
    redundancy: usize,
    // Messages that were received before the group was
    // started. Those will be "replayed" once a start message
    // is received.
    pre_start_msgs: Vec<BastionMessage>,
    started: bool,
}

#[derive(Debug, Clone)]
/// A "reference" to a children group, allowing to communicate
/// with it.
pub struct ChildrenRef {
    id: BastionId,
    sender: Sender,
    children: Vec<ChildRef>,
}

pub(crate) struct Child {
    bcast: Broadcast,
    // The future that this child is executing.
    exec: Exec,
    // A lock behind which is the child's context state.
    // This is used to store the messages that were received
    // for the child's associated future to be able to
    // retrieve them.
    state: Qutex<ContextState>,
    // Messages that were received before the child was
    // started. Those will be "replayed" once a start message
    // is received.
    pre_start_msgs: Vec<BastionMessage>,
    started: bool,
}

#[derive(Debug, Clone)]
/// A "reference" to an element of a children group, allowing to
/// communicate with it.
pub struct ChildRef {
    id: BastionId,
    sender: Sender,
}

impl Children {
    pub(crate) fn new(
        init: Box<dyn Closure>,
        bcast: Broadcast,
        supervisor: SupervisorRef,
        redundancy: usize,
    ) -> Self {
        let launched = FxHashMap::default();
        let pre_start_msgs = Vec::new();
        let started = false;

        let mut children = Children {
            bcast,
            supervisor,
            launched,
            init,
            redundancy,
            pre_start_msgs,
            started,
        };

        children.new_elems();

        children
    }

    fn new_elems(&mut self) {
        for _ in 0..self.redundancy {
            let parent = Parent::children(self.as_ref());
            let bcast = Broadcast::new(parent);
            // TODO: clone or ref?
            let id = bcast.id().clone();
            let sender = bcast.sender().clone();

            let child_ref = ChildRef::new(id.clone(), sender.clone());
            let children = self.as_ref();
            let supervisor = self.supervisor.clone();

            let state = ContextState::new();
            let state = Qutex::new(state);

            let ctx =
                BastionContext::new(id.clone(), child_ref, children, supervisor, state.clone());
            let exec = AssertUnwindSafe((self.init)(ctx).0).catch_unwind();

            self.bcast.register(&bcast);

            let child = Child::new(exec, bcast, state);
            let launched = Proc::spawn(child.run());

            self.launched.insert(id, (sender, launched));
        }
    }

    pub(crate) async fn reset(&mut self, bcast: Broadcast, supervisor: SupervisorRef) {
        // TODO: stop or kill?
        self.kill().await;

        self.bcast = bcast;
        self.supervisor = supervisor;

        self.new_elems();
    }

    pub(crate) fn id(&self) -> &BastionId {
        self.bcast.id()
    }

    pub(crate) fn bcast(&self) -> &Broadcast {
        &self.bcast
    }

    pub(crate) fn as_ref(&self) -> ChildrenRef {
        // TODO: clone or ref?
        let id = self.bcast.id().clone();
        let sender = self.bcast.sender().clone();

        let mut children = Vec::with_capacity(self.launched.len());
        for (id, (sender, _)) in &self.launched {
            // TODO: clone or ref?
            let child = ChildRef::new(id.clone(), sender.clone());
            children.push(child);
        }

        ChildrenRef::new(id, sender, children)
    }

    async fn stop(&mut self) {
        self.bcast.stop_children();

        let launched = self.launched.drain().map(|(_, (_, launched))| launched);
        FuturesUnordered::from_iter(launched)
            .collect::<Vec<_>>()
            .await;
    }

    async fn kill(&mut self) {
        self.bcast.kill_children();

        let launched = self.launched.drain().map(|(_, (_, launched))| launched);
        FuturesUnordered::from_iter(launched)
            .collect::<Vec<_>>()
            .await;
    }

    fn stopped(&mut self) {
        self.bcast.stopped();
    }

    fn faulted(&mut self) {
        self.bcast.faulted();
    }

    async fn handle(&mut self, msg: BastionMessage) -> Result<(), ()> {
        match msg {
            BastionMessage::Start => unreachable!(),
            BastionMessage::Stop => {
                self.stop().await;
                self.stopped();

                return Err(());
            }
            BastionMessage::Kill => {
                self.kill().await;
                self.stopped();

                return Err(());
            }
            // FIXME
            BastionMessage::Deploy(_) => unimplemented!(),
            // FIXME
            BastionMessage::Prune { .. } => unimplemented!(),
            // FIXME
            BastionMessage::SuperviseWith(_) => unimplemented!(),
            BastionMessage::Tell(_) => {
                self.bcast.send_children(msg);
            }
            BastionMessage::Stopped { id } => {
                // FIXME: Err if false?
                if self.launched.contains_key(&id) {
                    // TODO: stop or kill?
                    self.kill().await;
                    self.stopped();

                    return Err(());
                }
            }
            BastionMessage::Faulted { id } => {
                // FIXME: Err if false?
                if self.launched.contains_key(&id) {
                    // TODO: stop or kill?
                    self.kill().await;
                    self.faulted();

                    return Err(());
                }
            }
        }

        Ok(())
    }

    pub(crate) async fn run(mut self) -> Self {
        loop {
            match poll!(&mut self.bcast.next()) {
                // TODO: Err if started == true?
                Poll::Ready(Some(BastionMessage::Start)) => {
                    self.started = true;

                    let msg = BastionMessage::start();
                    self.bcast.send_children(msg);

                    let msgs = self.pre_start_msgs.drain(..).collect::<Vec<_>>();
                    self.pre_start_msgs.shrink_to_fit();

                    for msg in msgs {
                        if self.handle(msg).await.is_err() {
                            return self;
                        }
                    }
                }
                Poll::Ready(Some(msg)) if !self.started => {
                    self.pre_start_msgs.push(msg);
                }
                Poll::Ready(Some(msg)) => {
                    if self.handle(msg).await.is_err() {
                        return self;
                    }
                }
                Poll::Ready(None) => {
                    // TODO: stop or kill?
                    self.kill().await;
                    self.faulted();

                    return self;
                }
                Poll::Pending => pending!(),
            }
        }
    }
}

impl ChildrenRef {
    fn new(id: BastionId, sender: Sender, children: Vec<ChildRef>) -> Self {
        ChildrenRef {
            id,
            sender,
            children,
        }
    }

    /// Returns a list of [`ChildRef`] referencing the elements
    /// of the children group this `ChildrenRef` is referencing.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|_| async { Ok(()) }.into(), 1).unwrap();
    /// let elems: &[ChildRef] = children_ref.elems();
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    ///
    /// [`ChildRef`]: children/struct.ChildRef.html
    pub fn elems(&self) -> &[ChildRef] {
        &self.children
    }

    /// Sends a message to the children group this `ChildrenRef`
    /// is referencing which will then send it to all of its
    /// elements.
    ///
    /// An alternative would be to use [`elems`] to get all the
    /// elements of the group and then send the message to all
    /// of them.
    ///
    /// This method returns `()` if it succeeded, or `Err(msg)`
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `msg` - The message to send.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|_| async { Ok(()) }.into(), 1).unwrap();
    /// let msg = "A message containing data.";
    /// children_ref.broadcast(msg).expect("Couldn't send the message.");
    ///
    ///     # Bastion::children(|ctx: BastionContext|
    ///         # async move {
    /// // And then in every of the children group's elements' futures...
    /// message! { ctx.recv().await?,
    ///     ref msg: &'static str => {
    ///         assert_eq!(msg, &"A message containing data.");
    ///     },
    ///     // We are only sending a `&'static str` in this
    ///     // example, so we know that this won't happen...
    ///     _: _ => (),
    /// }
    ///             #
    ///             # Ok(())
    ///         # }.into(),
    ///         # 1,
    ///     # ).unwrap();
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    ///
    /// [`elems`]: #method.elems
    pub fn broadcast<M: Message>(&self, msg: M) -> Result<(), M> {
        let msg = BastionMessage::broadcast(msg);
        // FIXME: panics?
        self.send(msg).map_err(|err| err.into_msg().unwrap())
    }

    /// Sends a message to the children group this `ChildrenRef`
    /// is referencing to tell it to stop all of its running
    /// elements.
    ///
    /// This method returns `()` if it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|_| async { Ok(()) }.into(), 1).unwrap();
    /// children_ref.stop().expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn stop(&self) -> Result<(), ()> {
        let msg = BastionMessage::stop();
        self.send(msg).map_err(|_| ())
    }

    /// Sends a message to the children group this `ChildrenRef`
    /// is referencing to tell it to kill all of its running
    /// elements.
    ///
    /// This method returns `()` if it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|_| async { Ok(()) }.into(), 1).unwrap();
    /// children_ref.kill().expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn kill(&self) -> Result<(), ()> {
        let msg = BastionMessage::kill();
        self.send(msg).map_err(|_| ())
    }

    pub(crate) fn send(&self, msg: BastionMessage) -> Result<(), BastionMessage> {
        self.sender
            .unbounded_send(msg)
            .map_err(|err| err.into_inner())
    }
}

impl Child {
    fn new(exec: Exec, bcast: Broadcast, state: Qutex<ContextState>) -> Self {
        let pre_start_msgs = Vec::new();
        let started = false;

        let child = Child {
            bcast,
            exec,
            state,
            pre_start_msgs,
            started,
        };

        child
    }

    fn stopped(&mut self) {
        self.bcast.stopped();
    }

    fn faulted(&mut self) {
        self.bcast.faulted();
    }

    async fn handle(&mut self, msg: BastionMessage) -> Result<(), ()> {
        match msg {
            BastionMessage::Start => unreachable!(),
            BastionMessage::Stop | BastionMessage::Kill => {
                self.stopped();

                return Err(());
            }
            // FIXME
            BastionMessage::Deploy(_) => unimplemented!(),
            // FIXME
            BastionMessage::Prune { .. } => unimplemented!(),
            // FIXME
            BastionMessage::SuperviseWith(_) => unimplemented!(),
            BastionMessage::Tell(msg) => {
                let mut state = self.state.clone().lock_async().await.map_err(|_| ())?;
                state.push_msg(msg);
            }
            // FIXME
            BastionMessage::Stopped { .. } => unimplemented!(),
            // FIXME
            BastionMessage::Faulted { .. } => unimplemented!(),
        }

        Ok(())
    }

    async fn run(mut self) {
        loop {
            match poll!(&mut self.bcast.next()) {
                // TODO: Err if started == true?
                Poll::Ready(Some(BastionMessage::Start)) => {
                    self.started = true;

                    let msgs = self.pre_start_msgs.drain(..).collect::<Vec<_>>();
                    self.pre_start_msgs.shrink_to_fit();

                    for msg in msgs {
                        if self.handle(msg).await.is_err() {
                            return;
                        }
                    }

                    continue;
                }
                Poll::Ready(Some(msg)) if !self.started => {
                    self.pre_start_msgs.push(msg);

                    continue;
                }
                Poll::Ready(Some(msg)) => {
                    if self.handle(msg).await.is_err() {
                        return;
                    }

                    continue;
                }
                Poll::Ready(None) => {
                    self.faulted();

                    return;
                }
                Poll::Pending => (),
            }

            if !self.started {
                pending!();

                continue;
            }

            if let Poll::Ready(res) = poll!(&mut self.exec) {
                match res {
                    Ok(Ok(())) => return self.stopped(),
                    Ok(Err(())) | Err(_) => return self.faulted(),
                }
            }

            pending!();
        }
    }
}

impl ChildRef {
    fn new(id: BastionId, sender: Sender) -> ChildRef {
        ChildRef { id, sender }
    }

    /// Sends a message to the child this `ChildRef` is referencing.
    ///
    /// This method returns `()` if it succeeded, or `Err(msg)`
    /// otherwise.
    ///
    /// # Argument
    ///
    /// * `msg` - The message to send.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|_| async { Ok(()) }.into(), 1).unwrap();
    ///     # let child_ref = &children_ref.elems()[0];
    /// let msg = "A message containing data.";
    /// child_ref.send_msg(msg).expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn send_msg<M: Message>(&self, msg: M) -> Result<(), M> {
        let msg = BastionMessage::tell(msg);
        // FIXME: panics?
        self.send(msg).map_err(|msg| msg.into_msg().unwrap())
    }

    /// Sends a message to the child this `ChildRef` is referencing
    /// to tell it to stop its execution.
    ///
    /// This method returns `()` if it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|_| async { Ok(()) }.into(), 1).unwrap();
    ///     # let child_ref = &children_ref.elems()[0];
    /// child_ref.stop().expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn stop(&self) -> Result<(), ()> {
        let msg = BastionMessage::stop();
        self.send(msg).map_err(|_| ())
    }

    /// Sends a message to the child this `ChildRef` is referencing
    /// to tell it to suicide.
    ///
    /// This method returns `()` if it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|_| async { Ok(()) }.into(), 1).unwrap();
    ///     # let child_ref = &children_ref.elems()[0];
    /// child_ref.kill().expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn kill(&self) -> Result<(), ()> {
        let msg = BastionMessage::kill();
        self.send(msg).map_err(|_| ())
    }

    pub(crate) fn send(&self, msg: BastionMessage) -> Result<(), BastionMessage> {
        self.sender
            .unbounded_send(msg)
            .map_err(|err| err.into_inner())
    }
}

impl Debug for Children {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        fmt.debug_struct("Children")
            .field("bcast", &self.bcast)
            .field("supervisor", &self.supervisor)
            .field("launched", &self.launched)
            .field("init", &"Closure")
            .field("redundancy", &self.redundancy)
            .field("pre_start_msgs", &self.pre_start_msgs)
            .field("started", &self.started)
            .finish()
    }
}

impl Debug for Child {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        fmt.debug_struct("Child")
            .field("bcast", &self.bcast)
            .field("exec", &"Exec")
            .field("state", &self.state)
            .field("pre_start_msgs", &self.pre_start_msgs)
            .field("started", &self.started)
            .finish()
    }
}
