use crate::prelude::*;
use async_std::prelude::FutureExt as AsyncFutureExt;
use futures::future::{lazy, AbortHandle};
use std::future::Future;

use std::time::Duration;

fn task_future<U, T>(
  task: impl FnOnce(U, T) + 'static,
  state: T,
  delay: Option<Duration>,
) -> (U, impl Future<Output = ()>)
where
  U: SubscriptionLike + Default + Clone,
{
  let subscription = U::default();
  let c_subscription = subscription.clone();
  let fut = lazy(|_| {
    if !c_subscription.is_closed() {
      task(c_subscription, state)
    }
  })
  .delay(delay.unwrap_or_default());

  (subscription, fut)
}

/// A Scheduler is an object to order task and schedule their execution.
pub trait SharedScheduler {
  fn spawn<Fut>(&self, future: Fut, subscription: &mut SharedSubscription)
  where
    Fut: Future<Output = ()> + Send + 'static;

  fn schedule<T: Send + 'static>(
    &self,
    task: impl FnOnce(SharedSubscription, T) + Send + 'static,
    delay: Option<Duration>,
    state: T,
  ) -> SharedSubscription {
    let (mut subscription, fut) = task_future(task, state, delay);
    self.spawn(fut, &mut subscription);
    subscription
  }
}

pub trait LocalScheduler {
  fn spawn<Fut>(&self, future: Fut, subscription: &mut LocalSubscription)
  where
    Fut: Future<Output = ()> + 'static;

  fn schedule<T: 'static>(
    &self,
    task: impl FnOnce(LocalSubscription, T) + 'static,
    delay: Option<Duration>,
    state: T,
  ) -> LocalSubscription {
    let (mut subscription, fut) = task_future(task, state, delay);
    self.spawn(fut, &mut subscription);
    subscription
  }
}

pub struct SpawnHandle {
  handle: AbortHandle,
  is_closed: bool,
}

impl SpawnHandle {
  #[inline]
  pub fn new(handle: AbortHandle) -> Self {
    SpawnHandle {
      handle,
      is_closed: false,
    }
  }
}

impl SubscriptionLike for SpawnHandle {
  fn unsubscribe(&mut self) {
    self.is_closed = true;
    self.handle.abort();
  }

  #[inline]
  fn is_closed(&self) -> bool { self.is_closed }

  #[inline]
  fn inner_addr(&self) -> *const () {
    ((&self.handle) as *const _) as *const ()
  }
}

#[cfg(feature = "futures-scheduler")]
mod futures_scheduler {
  use super::*;
  use futures::{
    executor::{LocalSpawner, ThreadPool},
    task::{LocalSpawnExt, SpawnExt},
    FutureExt,
  };

  impl SharedScheduler for ThreadPool {
    fn spawn<Fut>(&self, future: Fut, subscription: &mut SharedSubscription)
    where
      Fut: Future<Output = ()> + Send + 'static,
    {
      let (f, handle) = futures::future::abortable(future);
      SpawnExt::spawn(self, f.map(|_| ())).unwrap();
      subscription.add(SpawnHandle::new(handle))
    }
  }

  impl LocalScheduler for LocalSpawner {
    fn spawn<Fut>(&self, future: Fut, subscription: &mut LocalSubscription)
    where
      Fut: Future<Output = ()> + 'static,
    {
      let (f, handle) = futures::future::abortable(future);
      self.spawn_local(f.map(|_| ())).unwrap();
      subscription.add(SpawnHandle::new(handle))
    }
  }
}

#[cfg(feature = "tokio-scheduler")]
mod tokio_scheduler {
  use super::*;
  use std::sync::Arc;
  use tokio::runtime::Runtime;

  fn rt_spawn<Fut>(
    rt: &Runtime,
    future: Fut,
    subscription: &mut SharedSubscription,
  ) where
    Fut: Future<Output = ()> + Send + 'static,
  {
    let (f, handle) = futures::future::abortable(future);
    subscription.add(SpawnHandle::new(handle));
    rt.spawn(f);
  }

  impl SharedScheduler for Runtime {
    fn spawn<Fut>(&self, future: Fut, subscription: &mut SharedSubscription)
    where
      Fut: Future<Output = ()> + Send + 'static,
    {
      rt_spawn(self, future, subscription)
    }
  }

  impl SharedScheduler for Arc<Runtime> {
    fn spawn<Fut>(&self, future: Fut, subscription: &mut SharedSubscription)
    where
      Fut: Future<Output = ()> + Send + 'static,
    {
      rt_spawn(self, future, subscription)
    }
  }
}

#[cfg(all(test, feature = "tokio-scheduler"))]
mod test {
  extern crate test;
  use crate::prelude::*;
  use futures::executor::{LocalPool, ThreadPool};
  use std::sync::{Arc, Mutex};
  use test::Bencher;

  fn waste_time(v: u32) -> u32 {
    (0..v)
      .into_iter()
      .map(|index| (0..index).sum::<u32>().min(u32::MAX / v))
      .sum()
  }

  #[bench]
  fn pool(b: &mut Bencher) {
    let last = Arc::new(Mutex::new(0));
    b.iter(|| {
      let c_last = last.clone();
      let pool = ThreadPool::new().unwrap();
      observable::from_iter(0..1000)
        .observe_on(pool)
        .map(waste_time)
        .to_shared()
        .subscribe(move |v| *c_last.lock().unwrap() = v);

      // todo: no way to wait all task has finished in `ThreadPool`.

      *last.lock().unwrap()
    })
  }

  #[bench]
  fn local_thread(b: &mut Bencher) {
    let last = Arc::new(Mutex::new(0));
    b.iter(|| {
      let c_last = last.clone();
      let mut local = LocalPool::new();
      observable::from_iter(0..1000)
        .observe_on(local.spawner())
        .map(waste_time)
        .subscribe(move |v| *c_last.lock().unwrap() = v);
      local.run();
      *last.lock().unwrap()
    })
  }

  #[bench]
  fn tokio_basic(b: &mut Bencher) {
    use tokio::runtime;
    let last = Arc::new(Mutex::new(0));
    b.iter(|| {
      let c_last = last.clone();
      let local = runtime::Builder::new().basic_scheduler().build().unwrap();

      observable::from_iter(0..1000)
        .observe_on(local)
        .map(waste_time)
        .to_shared()
        .subscribe(move |v| *c_last.lock().unwrap() = v);

      // todo: no way to wait all task has finished in `Tokio` Scheduler.
      *last.lock().unwrap()
    })
  }

  #[bench]
  fn tokio_thread(b: &mut Bencher) {
    use tokio::runtime;
    let last = Arc::new(Mutex::new(0));
    b.iter(|| {
      let c_last = last.clone();
      let pool = runtime::Builder::new()
        .threaded_scheduler()
        .build()
        .unwrap();
      observable::from_iter(0..1000)
        .observe_on(pool)
        .map(waste_time)
        .to_shared()
        .subscribe(move |v| *c_last.lock().unwrap() = v);

      // todo: no way to wait all task has finished in `Tokio` Scheduler.

      *last.lock().unwrap()
    })
  }
}
