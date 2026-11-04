//! The bounded Wayland event pump. One poll waits for compositor events, a
//! command wake and the screencast stream's wake together.
use super::Commands;
use super::state::State;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use wayland_client::Connection;

pub(super) fn pump(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
) -> anyhow::Result<()> {
    pump_wake(c, q, s, 50, None, None)
}

fn complete_wayland_io<T>(
    result: Result<T, wayland_client::backend::WaylandError>,
) -> anyhow::Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(wayland_client::backend::WaylandError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            // Another coordinated event queue can consume socket readiness.
            // The next pump retries reads/flushes without losing pending events.
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Dispatch one batch of compositor events, waiting up to `timeout_ms` for it,
/// for a command wake or for the screencast stream's wake. Neither wake is
/// cleared for the stream: its owner drains it.
pub(super) fn pump_wake(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    timeout_ms: i32,
    commands: Option<&Commands>,
    stream: Option<BorrowedFd<'_>>,
) -> anyhow::Result<()> {
    let dispatched = q.dispatch_pending(s)?;
    // A full Wayland socket needs POLLOUT; never block while holding a read
    // guard or mistake command-only readiness for readable Wayland bytes.
    let want_write = match c.flush() {
        Ok(()) => false,
        Err(wayland_client::backend::WaylandError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            true
        }
        Err(error) => return Err(error.into()),
    };
    // Pending callbacks can already satisfy negotiation/readback. Recheck the
    // caller's condition before entering another potentially long poll; the
    // Wayland socket may now legitimately be quiet after those events.
    if dispatched > 0 {
        s.validate_output()?;
        return Ok(());
    }
    if let Some(guard) = q.prepare_read() {
        poll_prepared_wayland(c, q, s, guard, timeout_ms, (commands, stream), want_write)?;
    }
    q.dispatch_pending(s)?;
    s.validate_output()?;
    Ok(())
}

fn poll_prepared_wayland(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    guard: wayland_client::backend::ReadEventsGuard,
    timeout_ms: i32,
    (commands, stream): (Option<&Commands>, Option<BorrowedFd<'_>>),
    want_write: bool,
) -> anyhow::Result<()> {
    // A different coordinated queue can read between dispatch_pending and
    // prepare_read, enqueueing our callbacks while draining socket readiness.
    // Once this guard is acquired, the backend prevents another socket read;
    // checking again here closes that window before any potentially long poll.
    if q.dispatch_pending(s)? > 0 {
        drop(guard);
        return Ok(());
    }
    let mut fds = [
        libc::pollfd {
            fd: c.as_fd().as_raw_fd(),
            events: libc::POLLIN | if want_write { libc::POLLOUT } else { 0 },
            revents: 0,
        },
        libc::pollfd {
            fd: commands.map_or(-1, |c| c.wake.as_raw_fd()),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stream.map_or(-1, |fd| fd.as_raw_fd()),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: all entries are initialized and valid for this call; -1 is ignored.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout_ms) };
    if n < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    if n > 0 && fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
        complete_wayland_io(guard.read())?;
    } else {
        // Cancels prepared read on command wakes, deadlines, EINTR and POLLOUT.
        drop(guard);
    }
    if fds[1].revents & libc::POLLIN != 0 {
        commands.unwrap().clear_wake();
    }
    if want_write && fds[0].revents & libc::POLLOUT != 0 {
        complete_wayland_io(c.flush())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Command;
    use super::*;
    use ironrdp_server::KeyboardEvent;
    use std::{
        io::{Read, Write},
        os::{
            fd::{FromRawFd, OwnedFd},
            unix::net::UnixStream,
        },
        time::{Duration, Instant},
    };
    use wayland_client::Proxy;
    #[test]
    fn pending_wayland_completion_is_dispatched_without_an_extra_timeout() {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut queue = connection.new_event_queue();
        let callback = connection.display().sync(&queue.handle(), ());
        connection.flush().unwrap();
        let mut reply = Vec::new();
        reply.extend_from_slice(&callback.id().protocol_id().to_ne_bytes());
        reply.extend_from_slice(&(12u32 << 16).to_ne_bytes());
        reply.extend_from_slice(&1u32.to_ne_bytes());
        compositor.write_all(&reply).unwrap();
        queue.prepare_read().unwrap().read().unwrap();
        let mut state = State::default();
        let started = Instant::now();
        pump_wake(&connection, &mut queue, &mut state, 1000, None, None).unwrap();
        assert!(state.synced);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a completed callback must not consume the next capture deadline"
        );
    }
    #[test]
    fn coordinated_empty_wayland_read_and_full_flush_are_retryable() {
        let (socket, _compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue::<State>();
        // Reading after another queue drained readiness is a normal condition.
        complete_wayland_io(queue.prepare_read().unwrap().read()).unwrap();
        complete_wayland_io::<()>(Err(wayland_client::backend::WaylandError::Io(
            std::io::Error::from(std::io::ErrorKind::WouldBlock),
        )))
        .unwrap();
        assert!(
            complete_wayland_io::<()>(Err(wayland_client::backend::WaylandError::Io(
                std::io::Error::from(std::io::ErrorKind::BrokenPipe),
            )))
            .is_err()
        );
        let mut state = State::default();
        pump_wake(
            &connection,
            &mut connection.new_event_queue(),
            &mut state,
            0,
            None,
            None,
        )
        .unwrap();
    }
    #[test]
    fn foreign_queue_read_between_dispatch_and_prepare_does_not_sleep_on_drained_socket() {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut capture = connection.new_event_queue::<State>();
        let clipboard = connection.clone().new_event_queue::<State>();
        let callback = connection.display().sync(&capture.handle(), ());
        connection.flush().unwrap();
        let mut state = State::default();
        assert_eq!(capture.dispatch_pending(&mut state).unwrap(), 0);
        let mut reply = Vec::new();
        reply.extend_from_slice(&callback.id().protocol_id().to_ne_bytes());
        reply.extend_from_slice(&(12u32 << 16).to_ne_bytes());
        reply.extend_from_slice(&1u32.to_ne_bytes());
        compositor.write_all(&reply).unwrap();
        // Deterministically model the clipboard reader winning immediately
        // after capture's first dispatch and before its prepared-read guard.
        clipboard.prepare_read().unwrap().read().unwrap();
        let mut fd = libc::pollfd {
            fd: connection.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd is initialized and refers to the live connection.
        assert_eq!(
            unsafe { libc::poll(&mut fd, 1, 0) },
            0,
            "foreign reader drained socket readiness"
        );
        let guard = capture.prepare_read().unwrap();
        let started = Instant::now();
        poll_prepared_wayland(
            &connection,
            &mut capture,
            &mut state,
            guard,
            1000,
            (None, None),
            false,
        )
        .unwrap();
        // pump_wake dispatches again after its prepared-read polling stage.
        capture.dispatch_pending(&mut state).unwrap();
        assert!(
            state.synced,
            "completion queued by the foreign reader must dispatch"
        );
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "queued completion must not wait for quiet socket readiness"
        );
        // The early return canceled the guard; later prepared reads still work.
        drop(capture.prepare_read().unwrap());
    }
    #[test]
    fn command_and_stream_wakes_end_the_wait_without_wayland_readiness() {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut events = connection.new_event_queue();
        let queue = Commands::new().unwrap();
        let producer = queue.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer
                .try_send(Command::Key(KeyboardEvent::Pressed {
                    code: 30,
                    extended: false,
                }))
                .unwrap();
        });
        let mut state = State::default();
        let started = Instant::now();
        // No compositor message is sent; only the command eventfd is readable.
        pump_wake(
            &connection,
            &mut events,
            &mut state,
            1000,
            Some(&queue),
            None,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(matches!(queue.pop(), Some(Command::Key(_))));
        worker.join().unwrap();
        // The screencast eventfd wakes the same poll and is left for its owner.
        // SAFETY: eventfd has no borrowed arguments; the fd is owned below.
        let stream = unsafe {
            OwnedFd::from_raw_fd(libc::eventfd(1, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))
        };
        let started = Instant::now();
        pump_wake(
            &connection,
            &mut events,
            &mut state,
            1000,
            Some(&queue),
            Some(stream.as_fd()),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        let mut value = 0u64;
        // SAFETY: eight writable bytes; the eventfd is nonblocking.
        let read = unsafe { libc::read(stream.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
        assert_eq!((read, value), (8, 1));
        drop(events);
        drop(connection);
        let mut bytes = [0u8; 16];
        compositor
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert_eq!(compositor.read(&mut bytes).unwrap(), 0);
    }
}
