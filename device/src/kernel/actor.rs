use super::{
    channel::{Channel, ChannelError, ChannelReceive, ChannelReceiver, ChannelSender},
    signal::{SignalFuture, SignalSlot},
    util::ImmediateFuture,
};
use core::cell::{RefCell, UnsafeCell};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use embassy::executor::{raw::Task, SpawnToken, Spawner};
use embassy::util::DropBomb;
use generic_array::GenericArray;
use heapless::{consts, ArrayLength};

/// Trait that each actor must implement. An Actor must specify a message type
/// it acts on, and an implementation of a message handler in `on_message`.
///
/// At run time, an Actor is held within an ActorContext, which contains the
/// embassy task and the message queues.
pub trait Actor: Sized {
    /// Max length of the message queue for this actor. Defaults to 1 for
    /// low footprint by default.
    type MessageQueueSize<'a>: ArrayLength<ActorMessage<'a, Self>>
        + ArrayLength<SignalSlot<Self::Response<'a>>>
    where
        Self: 'a,
    = consts::U1;

    /// The configuration that this actor will expect when mounted.
    type Configuration = ();

    /// The message type that this actor will handle in `on_message`.
    type Message<'a>: Sized
    where
        Self: 'a,
    = ();

    /// The response type that this actor will return in `on_message`.
    type Response<'a>: Sized + Send
    where
        Self: 'a,
    = ();

    /// Called to mount an actor into the system.
    ///
    /// The actor will be presented with both its own `Address<...>`.
    ///
    /// The default implementation does nothing.
    fn on_mount(&mut self, _: Self::Configuration) {}

    /// The future type returned in `on_start`, usually derived from an `async move` block
    /// in the implementation.
    ///
    /// The default type returns the ImmediateFuture that is ready immediately.
    type OnStartFuture<'a>: Future<Output = ()>
    where
        Self: 'a,
    = ImmediateFuture;

    /// Called when an actor is started, before it can process messages
    fn on_start(self: Pin<&'_ mut Self>) -> Self::OnStartFuture<'_>;

    /// The future type returned in `on_message`, usually derived from an `async move` block
    /// in the implementation. The return value of the future must be of the Response associated
    /// type.
    type OnMessageFuture<'a>: Future<Output = Self::Response<'a>>
    where
        Self: 'a;

    /// Handle an incoming message for this actor. The return value of the future must be of the
    /// Response associated type.
    fn on_message<'m>(
        self: Pin<&'m mut Self>,
        message: Self::Message<'m>,
    ) -> Self::OnMessageFuture<'m>;
}

/// A handle to another actor for dispatching messages.
///
/// Individual actor implementations may augment the `Address` object
/// when appropriate bounds are met to provide method-like invocations.
pub struct Address<'a, A>
where
    A: Actor + 'static,
{
    state: &'a ActorContext<'a, A>,
}

impl<'a, A: Actor> Address<'a, A> {
    pub fn new(state: &'a ActorContext<'a, A>) -> Self {
        Self { state }
    }
}

impl<'a, A: Actor> Address<'a, A> {
    /// Perform an _async_ message request to the actor behind this address.
    /// If an error occurs when enqueueing the message on the destination actor,
    /// an error is returned.
    ///
    /// The returned future complete when the receiving actor have processed the
    /// message, and the result from processing is made available when the future
    /// is ready.
    ///
    /// # Panics
    /// While the request message may contain non-static references, the user must
    /// ensure that the response to the request is fully `.await`'d before returning.
    /// Leaving an in-flight request dangling while references have gone out of lifetime
    /// scope will result in a panic.
    #[must_use = "The returned future must be awaited"]
    pub fn request<'m>(&self, message: A::Message<'m>) -> Result<RequestFuture<'a, A>, ActorError>
    where
        'a: 'm,
    {
        self.state.request(message)
    }

    /// Perform an message notification to the actor behind this address. If an error
    /// occurs when enqueueing the message on the destination actor, an error is returned.
    ///
    /// # Panics
    /// While the request message may contain non-static references, the user must
    /// ensure that the data passed lives as long as the actor.
    pub fn notify<'m>(&self, message: A::Message<'a>) -> Result<(), ActorError> {
        self.state.notify(message)
    }
}

impl<'a, A: Actor> Copy for Address<'a, A> {}

impl<'a, A: Actor> Clone for Address<'a, A> {
    fn clone(&self) -> Self {
        Self { state: self.state }
    }
}

pub struct MessageChannel<'a, T, N>
where
    N: ArrayLength<T>,
{
    channel: UnsafeCell<Channel<T, N>>,
    channel_sender: UnsafeCell<Option<ChannelSender<'a, T, N>>>,
    channel_receiver: UnsafeCell<Option<ChannelReceiver<'a, T, N>>>,
}

impl<'a, T, N> MessageChannel<'a, T, N>
where
    N: ArrayLength<T>,
{
    pub fn new() -> Self {
        Self {
            channel: UnsafeCell::new(Channel::new()),
            channel_sender: UnsafeCell::new(None),
            channel_receiver: UnsafeCell::new(None),
        }
    }

    pub fn initialize(&'a self) {
        let (sender, receiver) = unsafe { &mut *self.channel.get() }.split();
        unsafe { &mut *self.channel_sender.get() }.replace(sender);
        unsafe { &mut *self.channel_receiver.get() }.replace(receiver);
    }

    pub fn send<'m>(&self, message: T) -> Result<(), ChannelError> {
        let sender = unsafe { &mut *self.channel_sender.get() }.as_mut().unwrap();
        sender.try_send(message)
    }

    pub fn receive<'m>(&self) -> ChannelReceive<'m, 'a, T, N> {
        let receiver = unsafe { &*self.channel_receiver.get() }.as_ref().unwrap();
        receiver.receive()
    }
}

#[derive(Debug)]
pub enum ActorError {
    Channel(ChannelError),
    Signal(SignalError),
}

#[derive(Debug)]
pub enum SignalError {
    NoAvailableSignal,
}

pub struct ActorSpawner {
    spawner: Option<Spawner>,
}

impl ActorSpawner {
    pub fn idle() -> Self {
        Self { spawner: None }
    }
    pub fn new(spawner: Spawner) -> Self {
        Self {
            spawner: Some(spawner),
        }
    }

    pub fn spawn<A: Actor + 'static>(&self, actor: &'static ActorContext<'static, A>) {
        if let Some(spawner) = &self.spawner {
            spawner.spawn(actor.spawn()).unwrap();
        }
    }
}

enum ActorState<'a, A: Actor + 'static, N>
where
    A: Actor + 'static,
    N: ArrayLength<ActorMessage<'a, A>>,
{
    Idle,
    Start(A::OnStartFuture<'a>),
    Process,
    Receive(ChannelReceive<'a, 'a, ActorMessage<'a, A>, N>),
    Request(A::OnMessageFuture<'a>, *const SignalSlot<A::Response<'a>>),
    Notify(A::OnMessageFuture<'a>),
}

pub struct ActorFuture<'a, A>
where
    A: Actor + 'static,
{
    context: &'a ActorContext<'a, A>,
}

impl<'a, A> Future for ActorFuture<'a, A>
where
    A: Actor + 'static,
{
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.context.poll(cx)
    }
}

/// A context for an actor, providing signal and message queue. The QLEN parameter
/// is a const generic parameter, and needs to be at least 2 in order for the underlying
/// heapless queue to work. (Due to missing const generic expressions)
#[rustfmt::skip]
pub struct ActorContext<'a, A>
where
    A: Actor + 'static,
{
    task: Task<ActorFuture<'static, A>>,
    state: RefCell<Option<ActorState<'a, A, A::MessageQueueSize<'a>>>>,
    actor: UnsafeCell<A>,
    channel: MessageChannel<'a, ActorMessage<'a, A>, A::MessageQueueSize<'a>>,
    // NOTE: This wastes an extra signal because heapless requires at least 2 slots and
    // const generic expressions doesn't work in this case.
    signals: UnsafeCell<GenericArray<SignalSlot<A::Response<'a>>, A::MessageQueueSize<'a>>>,
}

impl<'a, A> ActorContext<'a, A>
where
    A: Actor,
{
    pub fn new(actor: A) -> Self {
        Self {
            task: Task::new(),
            state: RefCell::new(Some(ActorState::Idle)),
            actor: UnsafeCell::new(actor),
            channel: MessageChannel::new(),
            signals: UnsafeCell::new(Default::default()),
        }
    }

    /// Acquire a signal slot if there are any free available
    fn acquire_signal(&self) -> Result<&SignalSlot<A::Response<'a>>, SignalError> {
        let signals = unsafe { &mut *self.signals.get() };
        let mut i = 0;
        while i < signals.len() {
            if signals[i].acquire() {
                return Ok(&signals[i]);
            }
            i += 1;
        }
        Err(SignalError::NoAvailableSignal)
    }

    /// Perform a request to this actor. The result from processing the request will be provided when the future completes.
    /// The returned future _must_ be awaited before dropped. If it is not
    /// awaited, it will panic.
    fn request<'m>(&'a self, message: A::Message<'m>) -> Result<RequestFuture<'a, A>, ActorError>
    where
        'a: 'm,
    {
        let signal = self.acquire_signal()?;
        // Safety: This is OK because A::Message is Sized.
        let message = unsafe { core::mem::transmute_copy::<_, A::Message<'a>>(&message) };
        let message = ActorMessage::Request(message, signal);
        self.channel.send(message)?;
        let sig = SignalFuture::new(signal);
        Ok(RequestFuture::new(sig))
    }

    /// Perform a notification on this actor. The returned future _must_ be awaited before dropped. If it is not
    /// awaited, it will panic.
    fn notify<'m>(&'a self, message: A::Message<'a>) -> Result<(), ActorError>
    where
        'a: 'm,
    {
        let message = ActorMessage::Notify(message);

        Ok(self.channel.send(message)?)
    }

    /// Mount the underloying actor and initialize the channel.
    pub fn mount(
        &'static self,
        config: A::Configuration,
        spawner: &ActorSpawner,
    ) -> Address<'a, A> {
        unsafe { &mut *self.actor.get() }.on_mount(config);
        self.channel.initialize();

        spawner.spawn(self);
        Address::new(self)
    }

    pub(crate) fn spawn(&'static self) -> SpawnToken<ActorFuture<'static, A>> {
        let task = &self.task;
        let future = ActorFuture { context: self };
        let token = Task::spawn(task, move || future);
        token
    }

    // Poll this actor to make progress
    pub(crate) fn poll(&'a self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            let mut state = self.state.borrow_mut();
            match state.as_mut().unwrap() {
                ActorState::Idle => {
                    let fut = unsafe { Pin::new_unchecked(&mut *self.actor.get()) }.on_start();
                    state.replace(ActorState::Start(fut));
                }
                ActorState::Start(fut) => {
                    let r = unsafe { Pin::new_unchecked(fut) }.poll(cx);
                    if r.is_pending() {
                        return Poll::Pending;
                    } else {
                        state.replace(ActorState::Process);
                    }
                }
                ActorState::Process => {
                    state.replace(ActorState::Receive(self.channel.receive()));
                }
                ActorState::Receive(fut) => {
                    let r = unsafe { Pin::new_unchecked(fut) }.poll(cx);
                    match r {
                        Poll::Pending => {
                            return Poll::Pending;
                        }
                        Poll::Ready(message) => match message {
                            ActorMessage::Request(message, signal) => {
                                let fut = unsafe { Pin::new_unchecked(&mut *self.actor.get()) }
                                    .on_message(message);
                                state.replace(ActorState::Request(fut, signal));
                            }
                            ActorMessage::Notify(message) => {
                                let fut = unsafe { Pin::new_unchecked(&mut *self.actor.get()) }
                                    .on_message(message);
                                state.replace(ActorState::Notify(fut));
                            }
                        },
                    }
                }
                ActorState::Request(fut, signal) => {
                    let r = unsafe { Pin::new_unchecked(fut) }.poll(cx);
                    match r {
                        Poll::Pending => {
                            return Poll::Pending;
                        }
                        Poll::Ready(value) => {
                            unsafe { &**signal }.signal(value);
                            state.replace(ActorState::Process);
                        }
                    }
                }
                ActorState::Notify(fut) => {
                    let r = unsafe { Pin::new_unchecked(fut) }.poll(cx);
                    if r.is_pending() {
                        return Poll::Pending;
                    } else {
                        state.replace(ActorState::Process);
                    }
                }
            }
        }
    }

    // Used by test framework
    pub(crate) async fn process(&'a self) {
        // crate::log_stack!();
        let actor = unsafe { Pin::new_unchecked(&mut *self.actor.get()) };
        match self.channel.receive().await {
            ActorMessage::Request(message, signal) => {
                // crate::log_stack!();
                let value = actor.on_message(message).await;
                unsafe { &*signal }.signal(value);
            }
            ActorMessage::Notify(message) => {
                // crate::log_stack!();
                actor.on_message(message).await;
            }
        }
    }
}
pub struct RequestFuture<'a, A: Actor + 'static> {
    signal: SignalFuture<'a, A::Response<'a>>,
    bomb: Option<DropBomb>,
}

impl<'a, A: Actor> RequestFuture<'a, A> {
    pub fn new(signal: SignalFuture<'a, A::Response<'a>>) -> Self {
        Self {
            signal,
            bomb: Some(DropBomb::new()),
        }
    }
}

impl<'a, A: Actor> Future for RequestFuture<'a, A> {
    type Output = A::Response<'a>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = Pin::new(&mut self.signal).poll(cx);
        if result.is_ready() {
            self.bomb.take().unwrap().defuse();
            self.signal.release();
            return result;
        } else {
            return Poll::Pending;
        }
    }
}

impl From<SignalError> for ActorError {
    fn from(error: SignalError) -> ActorError {
        ActorError::Signal(error)
    }
}

impl From<ChannelError> for ActorError {
    fn from(error: ChannelError) -> ActorError {
        ActorError::Channel(error)
    }
}

pub enum ActorMessage<'m, A: Actor + 'm> {
    Request(A::Message<'m>, *const SignalSlot<A::Response<'m>>),
    Notify(A::Message<'m>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn test_multiple_notifications() {
        let spawner = ActorSpawner::idle();
        let actor = Box::leak(Box::new(ActorContext::new(DummyActor::new())));

        let address = actor.mount((), &spawner);

        let result_1 = address.notify(TestMessage(0));
        let result_2 = address.notify(TestMessage(1));

        assert!(result_1.is_ok());
        assert!(result_2.is_err());

        step_actor(actor);
        let result_2 = address.notify(TestMessage(1));
        assert!(result_2.is_ok());
    }

    #[test]
    fn test_multiple_requests() {
        let spawner = ActorSpawner::idle();
        let actor = Box::leak(Box::new(ActorContext::new(DummyActor::new())));

        let address = actor.mount((), &spawner);

        let result_fut_1 = address.request(TestMessage(0));
        let result_fut_2 = address.request(TestMessage(1));
        assert!(result_fut_1.is_ok());
        assert!(result_fut_2.is_err());

        let waker = futures::task::noop_waker_ref();
        let mut cx = std::task::Context::from_waker(waker);

        let mut fut_1 = result_fut_1.unwrap();

        while Pin::new(&mut fut_1).poll(&mut cx).is_pending() {
            step_actor(actor);
        }

        let result_fut_2 = address.request(TestMessage(1));
        assert!(result_fut_2.is_ok());

        let mut fut_2 = result_fut_2.unwrap();
        while Pin::new(&mut fut_2).poll(&mut cx).is_pending() {
            step_actor(actor);
        }
    }
}
