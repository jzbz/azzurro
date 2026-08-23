//! One queue for the requests that must not overtake each other.
//!
//! The command loop must never await a player. A player that has gone quiet
//! takes the full ten-second request timeout to say so, and every press made
//! in the meantime waits behind it — which is what a window that has stopped
//! responding actually is. So the arms that talk to a player hand the work to
//! a task and go back to reading the channel.
//!
//! For most of them that is the whole story: a settings page and a browse
//! screen are independent reads, and if two land out of order the second one
//! simply wins. A few cannot be treated that way. Removing queue track 3 and
//! then track 5 is not the same as removing 5 and then 3 — the indices are
//! positions in a list the first request shortens — and two writes to one
//! setting must leave the value the user asked for last, not whichever reply
//! came back last.
//!
//! Those go through here. `enter` is called on the command loop, which is
//! already serial, so the order of the line is exactly the order the presses
//! arrived in; the returned future is awaited inside the task, where waiting
//! costs nothing. Nothing here holds a lock across an await: the `Mutex` is
//! held only long enough to swap one channel end.

use std::sync::Mutex;

use tokio::sync::oneshot;

/// The place in line. Await it before doing the work.
pub struct Ticket {
    ahead: Option<oneshot::Receiver<()>>,
    /// Dropped when the work is done, which is what lets the next one go. A
    /// task that panics drops it too, so a failure cannot wedge the line.
    _done: oneshot::Sender<()>,
}

impl Ticket {
    /// Wait for everything asked for before this.
    pub async fn wait(&mut self) {
        if let Some(ahead) = self.ahead.take() {
            // An error means the task ahead was dropped without finishing,
            // which is the same news as finishing: it is no longer running.
            let _ = ahead.await;
        }
    }
}

/// Requests to one player, in the order they were made.
#[derive(Default)]
pub struct Lane(Mutex<Option<oneshot::Receiver<()>>>);

impl Lane {
    /// Take a place at the back of the line.
    pub fn enter(&self) -> Ticket {
        let (done, next) = oneshot::channel();
        // Whoever was last is now the one to wait for.
        let ahead = self.0.lock().unwrap().replace(next);
        Ticket { ahead, _done: done }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Three tasks started in a deliberately unhelpful order still run in the
    /// order they entered the lane.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_line_is_the_order_it_was_joined_in() {
        let lane = Lane::default();
        let order = Arc::new(Mutex::new(Vec::new()));

        // Entered on one thread, exactly as the command loop does it.
        let tickets: Vec<_> = (0..3).map(|i| (i, lane.enter())).collect();

        // Started back to front, and the first one made to dawdle, so nothing
        // but the lane can be producing the order below.
        let mut tasks = Vec::new();
        for (i, mut ticket) in tickets.into_iter().rev() {
            let order = order.clone();
            tasks.push(tokio::spawn(async move {
                ticket.wait().await;
                if i == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                }
                order.lock().unwrap().push(i);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    /// A task that dies without finishing releases the one behind it.
    #[tokio::test]
    async fn a_dropped_ticket_does_not_wedge_the_line() {
        let lane = Lane::default();
        let first = lane.enter();
        let mut second = lane.enter();
        drop(first);
        // Would hang if the line waited for a task that no longer exists.
        second.wait().await;
    }

    /// Nothing ahead of the first one.
    #[tokio::test]
    async fn the_first_in_line_does_not_wait() {
        let lane = Lane::default();
        let mut only = lane.enter();
        only.wait().await;
    }
}
