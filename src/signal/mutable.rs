use super::Signal;
use std;
// TODO use parking_lot ?
use std::sync::{Arc, Weak, Mutex, RwLock, MutexGuard};
// TODO use parking_lot ?
use std::sync::atomic::{AtomicBool, Ordering};
use futures_core::Async;
use futures_core::task::{Context, Waker};
use serde::{Serialize, Deserialize, Serializer, Deserializer};


struct MutableState<A> {
    value: A,
    senders: usize,
    // TODO use HashMap or BTreeMap instead ?
    receivers: Vec<Weak<MutableSignalState<A>>>,
}

impl<A> MutableState<A> {
    fn notify(&mut self, has_changed: bool) {
        self.receivers.retain(|receiver| {
            if let Some(receiver) = receiver.upgrade() {
                let mut lock = receiver.waker.lock().unwrap();

                if has_changed {
                    // TODO verify that this is correct
                    receiver.has_changed.store(true, Ordering::SeqCst);
                }

                if let Some(waker) = lock.take() {
                    drop(lock);
                    waker.wake();
                }

                true

            } else {
                false
            }
        });
    }
}

struct MutableSignalState<A> {
    has_changed: AtomicBool,
    waker: Mutex<Option<Waker>>,
    // TODO change this to Weak ?
    state: Arc<RwLock<MutableState<A>>>,
}

impl<A> MutableSignalState<A> {
    fn new(mutable_state: &Arc<RwLock<MutableState<A>>>) -> Arc<Self> {
        let state = Arc::new(MutableSignalState {
            has_changed: AtomicBool::new(true),
            waker: Mutex::new(None),
            state: mutable_state.clone(),
        });

        {
            let mut lock = mutable_state.write().unwrap();
            lock.receivers.push(Arc::downgrade(&state));
        }

        state
    }
}


pub struct Mutable<A>(Arc<RwLock<MutableState<A>>>);

impl<A> Mutable<A> {
    pub fn new(value: A) -> Self {
        Mutable(Arc::new(RwLock::new(MutableState {
            value,
            senders: 1,
            receivers: vec![],
        })))
    }

    pub fn replace(&self, value: A) -> A {
        let mut state = self.0.write().unwrap();

        let value = std::mem::replace(&mut state.value, value);

        state.notify(true);

        value
    }

    pub fn replace_with<F>(&self, f: F) -> A where F: FnOnce(&mut A) -> A {
        let mut state = self.0.write().unwrap();

        let new_value = f(&mut state.value);
        let value = std::mem::replace(&mut state.value, new_value);

        state.notify(true);

        value
    }

    pub fn swap(&self, other: &Mutable<A>) {
        // TODO can this dead lock ?
        let mut state1 = self.0.write().unwrap();
        let mut state2 = other.0.write().unwrap();

        std::mem::swap(&mut state1.value, &mut state2.value);

        state1.notify(true);
        state2.notify(true);
    }

    pub fn set(&self, value: A) {
        let mut state = self.0.write().unwrap();

        state.value = value;

        state.notify(true);
    }

    // TODO figure out a better name for this ?
    pub fn with_ref<B, F>(&self, f: F) -> B where F: FnOnce(&A) -> B {
        let state = self.0.read().unwrap();
        f(&state.value)
    }
}

impl<A: Copy> Mutable<A> {
    #[inline]
    pub fn get(&self) -> A {
        self.0.read().unwrap().value
    }

    #[inline]
    pub fn signal(&self) -> MutableSignal<A> {
        MutableSignal(MutableSignalState::new(&self.0))
    }
}

impl<A: Clone> Mutable<A> {
    #[inline]
    pub fn get_cloned(&self) -> A {
        self.0.read().unwrap().value.clone()
    }

    #[inline]
    pub fn signal_cloned(&self) -> MutableSignalCloned<A> {
        MutableSignalCloned(MutableSignalState::new(&self.0))
    }
}

impl<T> Serialize for Mutable<T> where T: Serialize {
    #[inline]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> where S: Serializer {
        self.0.read().unwrap().value.serialize(serializer)
    }
}

impl<'de, T> Deserialize<'de> for Mutable<T> where T: Deserialize<'de> {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: Deserializer<'de> {
        T::deserialize(deserializer).map(Mutable::new)
    }
}

// TODO can this be derived ?
impl<T: Default> Default for Mutable<T> {
    #[inline]
    fn default() -> Self {
        Mutable::new(Default::default())
    }
}

/*impl<A> Clone for Mutable<A> {
    #[inline]
    fn clone(&self) -> Self {
        self.0.write().unwrap().senders += 1;
        Mutable(self.0.clone())
    }
}*/

impl<A> Drop for Mutable<A> {
    #[inline]
    fn drop(&mut self) {
        let mut state = self.0.write().unwrap();

        state.senders -= 1;

        if state.senders == 0 && state.receivers.len() > 0 {
            state.notify(false);
            state.receivers = vec![];
        }
    }
}


// TODO remove it from receivers when it's dropped
pub struct MutableSignal<A>(Arc<MutableSignalState<A>>);

impl<A: Copy> Signal for MutableSignal<A> {
    type Item = A;

    fn poll_change(&mut self, cx: &mut Context) -> Async<Option<Self::Item>> {
        // TODO is this correct ?
        let lock = self.0.state.read().unwrap();

        // TODO verify that this is correct
        if self.0.has_changed.swap(false, Ordering::SeqCst) {
            Async::Ready(Some(lock.value))

        } else if lock.senders == 0 {
            Async::Ready(None)

        } else {
            // TODO is this correct ?
            *self.0.waker.lock().unwrap() = Some(cx.waker().clone());
            Async::Pending
        }
    }
}


// TODO it should have a single MutableSignal implementation for both Copy and Clone
// TODO remove it from receivers when it's dropped
pub struct MutableSignalCloned<A>(Arc<MutableSignalState<A>>);

impl<A: Clone> Signal for MutableSignalCloned<A> {
    type Item = A;

    // TODO code duplication with MutableSignal::poll
    fn poll_change(&mut self, cx: &mut Context) -> Async<Option<Self::Item>> {
        // TODO is this correct ?
        let lock = self.0.state.read().unwrap();

        // TODO verify that this is correct
        if self.0.has_changed.swap(false, Ordering::SeqCst) {
            Async::Ready(Some(lock.value.clone()))

        } else if lock.senders == 0 {
            Async::Ready(None)

        } else {
            // TODO is this correct ?
            *self.0.waker.lock().unwrap() = Some(cx.waker().clone());
            Async::Pending
        }
    }
}


struct Inner<A> {
    value: Option<A>,
    waker: Option<Waker>,
    dropped: bool,
}

impl<A> Inner<A> {
    fn notify(mut lock: MutexGuard<Self>) {
        if let Some(waker) = lock.waker.take() {
            drop(lock);
            waker.wake();
        }
    }
}

pub struct Sender<A> {
    inner: Weak<Mutex<Inner<A>>>,
}

impl<A> Sender<A> {
    pub fn send(&self, value: A) -> Result<(), A> {
        if let Some(inner) = self.inner.upgrade() {
            let mut inner = inner.lock().unwrap();

            inner.value = Some(value);

            Inner::notify(inner);

            Ok(())

        } else {
            Err(value)
        }
    }
}

impl<A> Drop for Sender<A> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let mut inner = inner.lock().unwrap();

            inner.dropped = true;

            Inner::notify(inner);
        }
    }
}


pub struct Receiver<A> {
    inner: Arc<Mutex<Inner<A>>>,
}

impl<A> Signal for Receiver<A> {
    type Item = A;

    #[inline]
    fn poll_change(&mut self, cx: &mut Context) -> Async<Option<Self::Item>> {
        let mut inner = self.inner.lock().unwrap();

        // TODO is this correct ?
        match inner.value.take() {
            None => if inner.dropped {
                Async::Ready(None)

            } else {
                inner.waker = Some(cx.waker().clone());
                Async::Pending
            },

            a => Async::Ready(a),
        }
    }
}

pub fn channel<A>(initial_value: A) -> (Sender<A>, Receiver<A>) {
    let inner = Arc::new(Mutex::new(Inner {
        value: Some(initial_value),
        waker: None,
        dropped: false,
    }));

    let sender = Sender {
        inner: Arc::downgrade(&inner),
    };

    let receiver = Receiver {
        inner,
    };

    (sender, receiver)
}