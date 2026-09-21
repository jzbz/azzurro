//! The small files this app keeps beside itself.
//!
//! Five of them — the players seen before, the searches made before, the
//! stations typed in by hand, the order the home screen's sections were put
//! in, and what each player's decoder last named. Each is small and each is
//! rewritten whole rather than edited. The first four were written with
//! `std::fs::write`, which truncates the file first and then
//! writes into it. A crash, a full disk or a machine losing power between
//! those two leaves nothing behind, and the stations file is the one where
//! that matters: it is typed in by hand and kept nowhere else, so what it
//! loses cannot be found again.
//!
//! So a write goes to a temp file beside the target and is renamed onto it,
//! the way the artwork cache already writes its files: a rename within one
//! directory is atomic, and a reader sees either the old file or the whole new
//! one. For the hand-typed file the bytes are pushed to the disk before the
//! rename as well, so the name cannot end up pointing at a file whose contents
//! never left the page cache.
//!
//! Two other things live here because they are the same problem in a different
//! coat.
//!
//! **Order.** [`Store::save`] hands over the whole file rather than a change
//! to it, and the disk write happens on a blocking thread. Only the newest
//! body ever reaches the disk, so an older snapshot cannot land last and undo
//! a Clear — which is what happened when each caller spawned its own blocking
//! write and the pool ran them in whatever order it liked.
//!
//! That is only as good as the order the bodies are handed over in, and
//! nothing here can supply it: a body read out of some state and staged after
//! that state's lock was let go can be overtaken by whatever changed it in
//! between, which is the same Clear undone by a different route. So a caller
//! stages while the state it read is still locked, and [`Store::stage`] is
//! built to allow that — it takes no I/O with it at all.
//!
//! **A file that will not read.** Invalid UTF-8, no permission, a directory
//! where the file should be: none of those mean the list is empty, and loading
//! them as empty meant the first change wrote that emptiness over whatever was
//! really there. Only [`std::io::ErrorKind::NotFound`] means empty. Anything
//! else seals the store — the run carries on with an empty list, and nothing
//! is written back — so the file is still there to be repaired by hand.
//! Whatever is typed in afterwards stays staged, so the window goes on showing
//! it; [`Store::sealed`] is how a caller finds out that it is all it will ever
//! be, and says so.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// How much trouble it is to be taken over getting the contents onto the disk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// The rename is enough. Right for the files this app can rebuild on its
    /// own: a list of addresses rediscovers itself, and asking the disk to
    /// flush on every status poll would be a real cost for it.
    Rename,
    /// The contents are flushed before the rename that publishes them. For a
    /// file nobody can retype.
    Synced,
}

/// One of those files, and whatever is waiting to go into it.
pub struct Store {
    /// Where it lives, or `None` on a machine with no config directory at all,
    /// where there is nothing to read and nowhere to write.
    path: Option<PathBuf>,
    durability: Durability,
    /// What the file should hold, until it holds it. The whole body and not a
    /// change to it, which is what lets a newer one simply replace it.
    ///
    /// The number beside it counts the hands-over, and it is there so a writer
    /// can tell its own body from one staged while it was out. A writer clears
    /// this only once the rename has landed and only if the number is still
    /// the one it carried out: until then this is what the file is about to
    /// hold, which is what [`Store::read`] has to keep answering with, and a
    /// write that fails leaves it here for the next one to carry.
    pending: Mutex<Option<(u64, String)>>,
    /// One writer at a time, so two of them cannot be part-way through the
    /// same temp file.
    writing: Mutex<()>,
    /// Whether the file was there and could not be read. See the note above:
    /// a sealed store writes nothing.
    sealed: AtomicBool,
    /// Whether the last write of this file did not reach the disk.
    ///
    /// Separate from `sealed` because it must not stop the next write: a full
    /// disk empties, a directory that could not be made can be made later, and
    /// the body is still staged for whoever tries next. It is here so that
    /// [`Self::sealed`] can answer the question its callers actually ask —
    /// whether what they are about to promise will survive the next start —
    /// rather than only the one about reading.
    failed: AtomicBool,
}

impl Store {
    /// A file of this name under `~/.config/azzurro`.
    pub fn named(name: &str, durability: Durability) -> Store {
        Store::at(
            dirs::config_dir().map(|dir| dir.join("azzurro").join(name)),
            durability,
        )
    }

    /// The same, at a path of the caller's choosing. For the tests, which have
    /// no business in the config directory of whoever runs them.
    fn at(path: Option<PathBuf>, durability: Durability) -> Store {
        Store {
            path,
            durability,
            pending: Mutex::new(None),
            writing: Mutex::new(()),
            sealed: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }
    }

    /// The file's contents, or `None` when there are none to be had.
    ///
    /// `None` means the file is not there *or* could not be read, and the
    /// caller cannot tell those apart on purpose: what it does about either is
    /// to start empty. The difference is kept here, where it decides whether
    /// anything may be written back.
    pub fn read(&self) -> Option<String> {
        // Whatever is waiting to be written is what the file holds, as far as
        // anyone asking is concerned. The writes happen off the loop, so a
        // station renamed and a pane closed and opened again is faster than
        // the disk; reading round the staged body would put the old name back
        // on screen and then save it.
        if let Some((_, waiting)) = self.pending.lock().unwrap().clone() {
            return Some(waiting);
        }

        let path = self.path.as_deref()?;
        match std::fs::read_to_string(path) {
            Ok(text) => {
                self.sealed.store(false, Ordering::Relaxed);
                Some(text)
            }
            // Nothing written down yet, which is the ordinary case at a first
            // start and not worth a word.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.sealed.store(false, Ordering::Relaxed);
                None
            }
            Err(e) => {
                tracing::warn!(
                    "cannot read {}: {e}. Leaving it alone; nothing will be written over it \
                     until it can be read again",
                    path.display()
                );
                self.sealed.store(true, Ordering::Relaxed);
                None
            }
        }
    }

    /// Whether nothing this run puts in this file will be in it at the next
    /// start.
    ///
    /// Three ways for that to be true, and a caller saying so out loud cannot
    /// tell them apart — which is the point: what it has to say is the same
    /// sentence either way.
    ///
    /// The file is there and could not be read, so nothing will be written
    /// back over it; that one answers for the last [`Self::read`]. Or there is
    /// no config directory on this machine at all, so `path` is `None` and
    /// [`Self::flush`] has nowhere to go — a store with nowhere to write can
    /// never keep anything. Or the last write was attempted and did not land,
    /// which says the next one probably will not either.
    ///
    /// That last one is a report of what already happened rather than a
    /// promise about what will: the write runs off the calling thread, so the
    /// failure it describes is the one before this. The first arrangement made
    /// over a full disk is still claimed; the second is not. Better than never
    /// saying it, and cheaper than holding the toast for a round trip to the
    /// disk.
    ///
    /// For the callers that have to say it out loud: the stations are typed in
    /// by hand and kept nowhere else, so a list that cannot be written is
    /// worth a word before more is typed into it, and Customize Home makes a
    /// claim about the next start every time it rearranges a screen.
    pub fn sealed(&self) -> bool {
        self.path.is_none()
            || self.sealed.load(Ordering::Relaxed)
            || self.failed.load(Ordering::Relaxed)
    }

    /// Say what the file should hold, without writing it.
    ///
    /// For state that is still being typed: the body is the file's as far as
    /// anything later is concerned, and whoever writes next writes this rather
    /// than what they arrived with.
    pub fn stage(&self, body: String) {
        let mut pending = self.pending.lock().unwrap();
        // One past whatever is there, so a writer that carried the old body
        // out finds a number it does not recognise and leaves this one alone.
        // Starting again from zero once a body has landed is safe: a writer
        // holds the lock on writing from the clone to the clear, so nothing it
        // could confuse this with is ever in flight.
        let stamp = pending
            .as_ref()
            .map_or(0, |(stamp, _)| stamp.wrapping_add(1));
        *pending = Some((stamp, body));
    }

    /// Write whatever is staged, off this thread.
    ///
    /// Cheap enough to call while the state it came from is still locked —
    /// it takes no I/O with it — which is the point: the command loop holds
    /// the browsing lock over most of what changes these files.
    pub fn sync(&'static self) {
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(|| self.flush());
            }
            // No runtime to hand it to: a test, or the last moments of a run
            // once the window has closed and nothing is waiting on this
            // thread anyway.
            Err(_) => self.flush(),
        }
    }

    /// Hand over the file's new contents and get them written.
    pub fn save(&'static self, body: String) {
        self.stage(body);
        self.sync();
    }

    /// Write whatever is staged, here and now.
    ///
    /// Called on the way out, where blocking is the whole intention: a body
    /// still waiting when the runtime is dropped goes down with it.
    pub fn flush(&self) {
        let _writing = self.writing.lock().unwrap();
        // Asked before the body is touched, and every way out below leaves it
        // where it is. A store with nowhere to write and a sealed one both
        // still have a session to serve: [`Self::read`] hands the staged body
        // back, so a station typed in over an unreadable file is at least
        // there for as long as the window is open. Taking it first meant the
        // pane came up empty again the moment it was reopened.
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if self.sealed.load(Ordering::Relaxed) {
            if self.pending.lock().unwrap().is_some() {
                tracing::warn!(
                    "not writing {}: it could not be read, so what is in it is not ours to replace",
                    path.display()
                );
            }
            return;
        }

        // Cloned rather than taken, and cleared at the end. Between here and
        // the rename the file still holds the *old* contents, so a reader that
        // found nothing staged would read round this body and hand back the
        // name that is being replaced — which is the one thing the check at
        // the top of `read` exists to prevent, and for the stations file that
        // window is a whole fsync wide. A write that fails leaves it staged
        // too, so the next `sync` carries it rather than losing it.
        let Some((stamp, body)) = self.pending.lock().unwrap().clone() else {
            // Another writer carried this state out already. What is staged
            // is always the whole file, so there is nothing left over.
            return;
        };

        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::debug!("cannot make {}: {e}", parent.display());
            // Written down rather than only logged, so that whoever is about
            // to promise this file will hold something can find out that the
            // last attempt to make it hold anything failed. The body stays
            // staged and the next `sync` still tries, which is why this is not
            // the seal above. See [`Self::sealed`].
            self.failed.store(true, Ordering::Relaxed);
            return;
        }
        if let Err(e) = self.write(path, &body) {
            tracing::debug!("cannot write {}: {e}", path.display());
            self.failed.store(true, Ordering::Relaxed);
            return;
        }
        self.failed.store(false, Ordering::Relaxed);

        // On the disk, so it is the file's to answer with rather than this
        // run's — unless something newer was staged while the write was out,
        // which is not in the file and would be lost with it.
        let mut pending = self.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|(newer, _)| *newer == stamp) {
            *pending = None;
        }
    }

    /// The temp file and the rename onto the target.
    fn write(&self, path: &Path, body: &str) -> std::io::Result<()> {
        let temp = temp_name(path);
        let out = self
            .fill(&temp, body)
            .and_then(|()| std::fs::rename(&temp, path));
        if out.is_err() {
            // Nothing reads it, and leaving it would mean a directory that
            // grows a file per failure.
            let _ = std::fs::remove_file(&temp);
            return out;
        }

        if self.durability == Durability::Synced
            && let Some(parent) = path.parent()
        {
            // The bytes are on the disk and the name that points at them is
            // not: a rename lives in the directory, and the directory has its
            // own cache. Without this a machine that loses power just after a
            // station is added comes back to the file's previous contents,
            // which is the same loss the flush above is there to prevent, one
            // level up. Only for the hand-typed file, so the three that
            // rebuild themselves go on paying nothing.
            //
            // Best effort: Windows will not open a directory at all, and there
            // is nothing to be done about that and nothing to tell anyone.
            let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
        }
        out
    }

    fn fill(&self, temp: &Path, body: &str) -> std::io::Result<()> {
        let mut file = std::fs::File::create(temp)?;
        file.write_all(body.as_bytes())?;
        if self.durability == Durability::Synced {
            // The rename publishes the file; this is what makes sure there is
            // something behind the name. Without it a crash just after the
            // rename can leave a file whose bytes were still in the page
            // cache — an empty stations file, which is the whole thing this
            // module exists to prevent.
            file.sync_all()?;
        }
        Ok(())
    }
}

/// Where a write of `path` assembles the new contents before publishing them.
///
/// Beside the target rather than in the system temp directory: a rename is
/// only atomic within one filesystem, and the target's own directory is the
/// one place guaranteed to be it.
///
/// The process id is in the name because nothing stops a second window of this
/// app running on the same account. Two of them sharing one temp name share
/// one inode: each `create` truncates what the other is part-way through
/// writing, and the rename publishes the mixture — for the stations file, a
/// list with lines from two writes in it, which is a hand-typed station gone.
fn temp_name(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.new", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own, removed when the guard goes.
    struct Dir(PathBuf);

    impl Dir {
        fn new(what: &str) -> Dir {
            let dir =
                std::env::temp_dir().join(format!("azzurro-store-{}-{what}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a temp directory");
            Dir(dir)
        }

        fn store(&self, durability: Durability) -> Store {
            Store::at(Some(self.0.join("list")), durability)
        }

        fn file(&self) -> PathBuf {
            self.0.join("list")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_write_leaves_the_contents_and_nothing_else() {
        let dir = Dir::new("plain");
        let store = dir.store(Durability::Synced);

        store.stage("one\n".to_owned());
        store.flush();
        assert_eq!(std::fs::read_to_string(dir.file()).unwrap(), "one\n");

        store.stage("two\n".to_owned());
        store.flush();
        assert_eq!(std::fs::read_to_string(dir.file()).unwrap(), "two\n");

        // The temp file is the mechanism, not a leftover.
        let left: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            left,
            vec!["list".to_owned()],
            "a stray file was left behind"
        );
    }

    #[test]
    fn a_write_that_cannot_be_made_leaves_the_old_file_whole() {
        // What `std::fs::write` could not do: it truncates the target and then
        // writes, so a failure part-way is an empty or half a file. Here the
        // failure is arranged by putting a directory where the temp file wants
        // to be, which is as good as a full disk for this purpose.
        let dir = Dir::new("failed");
        std::fs::write(dir.file(), "hand typed\n").expect("the file to be there");
        std::fs::create_dir_all(temp_name(&dir.file())).expect("the temp name taken");

        let store = dir.store(Durability::Synced);
        store.stage("replacement\n".to_owned());
        store.flush();

        assert_eq!(
            std::fs::read_to_string(dir.file()).unwrap(),
            "hand typed\n",
            "a write that could not be made took the old contents with it"
        );

        // And the body is still this session's: the rename is what makes a
        // body the file's, and it did not happen. Taken on the way in, it was
        // gone from memory as well — so reopening the pane read the old file,
        // showed the name the rename was meant to replace, and the next edit
        // from that stale list wrote the rename out of existence.
        assert_eq!(
            store.read().as_deref(),
            Some("replacement\n"),
            "a write that could not be made lost what it was asked to write"
        );

        // Which means the next write carries it, rather than there being
        // nothing left to carry.
        std::fs::remove_dir(temp_name(&dir.file())).expect("the temp name freed");
        store.flush();
        assert_eq!(
            std::fs::read_to_string(dir.file()).unwrap(),
            "replacement\n"
        );
    }

    #[test]
    fn a_read_while_a_write_is_out_gets_the_body_being_written() {
        // The window this closes: `flush` is on a blocking thread and, for the
        // stations file, spends a whole fsync inside it. Rename a station,
        // leave the pane — which is what starts the write — and open it again
        // before the disk has finished, and the load has to see the new name.
        // Reading round it handed back the old one, and the next edit from
        // that list wrote it back over the rename.
        let dir = Dir::new("mid-write");
        std::fs::write(dir.file(), "old name\n").expect("the file to be there");

        let store = std::sync::Arc::new(dir.store(Durability::Synced));
        store.stage("new name\n".to_owned());

        // The write is held part-way through by holding the lock it takes
        // first, which is the same standing still that an fsync is.
        let held = store.writing.lock().unwrap();
        let writer = {
            let store = store.clone();
            std::thread::spawn(move || store.flush())
        };
        // Whatever the writer is doing — waiting for the lock, or inside the
        // write once it has it — the answer is the same body.
        assert_eq!(store.read().as_deref(), Some("new name\n"));
        drop(held);
        writer.join().expect("the write to finish");

        assert_eq!(std::fs::read_to_string(dir.file()).unwrap(), "new name\n");
        assert_eq!(store.read().as_deref(), Some("new name\n"));
    }

    #[test]
    fn a_body_staged_while_a_write_is_out_is_not_cleared_by_it() {
        // The other half of not taking the body: a writer that cleared
        // whatever it found on the way out would drop a newer body that never
        // reached the disk.
        let dir = Dir::new("overtaken");
        let store = dir.store(Durability::Rename);

        store.stage("first\n".to_owned());
        {
            // As if it had been typed while the write was out.
            let _writing = store.writing.lock().unwrap();
            store.stage("second\n".to_owned());
        }
        store.flush();
        assert_eq!(std::fs::read_to_string(dir.file()).unwrap(), "second\n");
        assert_eq!(store.read().as_deref(), Some("second\n"));
    }

    #[test]
    fn the_temp_file_is_this_process_and_no_other() {
        // Two windows of this app on one account write the same file, and
        // nothing stops them. Sharing a temp name meant sharing an inode:
        // each truncated what the other was part-way through writing, and the
        // rename published the mixture.
        let name = temp_name(Path::new("/tmp/azzurro/stations"));
        assert_eq!(
            name,
            PathBuf::from(format!("/tmp/azzurro/stations.{}.new", std::process::id()))
        );
    }

    #[test]
    fn the_newest_body_is_the_one_that_lands() {
        // Two saves handed over in order, written by whoever gets there first.
        // This is the Clear that an older snapshot used to undo: the list is
        // emptied after a search is remembered, and the empty file has to be
        // what survives however the writers are scheduled.
        let dir = Dir::new("order");
        let store = dir.store(Durability::Rename);

        store.stage("van halen\n".to_owned());
        store.stage(String::new());
        store.flush();
        assert_eq!(std::fs::read_to_string(dir.file()).unwrap(), "");

        // And the second writer has nothing of its own to put back.
        store.flush();
        assert_eq!(std::fs::read_to_string(dir.file()).unwrap(), "");
    }

    #[test]
    fn what_is_waiting_to_be_written_is_what_is_read_back() {
        // The pane is opened again before the write it staged has landed,
        // which is an ordinary thing to do: the disk is slower than closing a
        // pane and opening it.
        let dir = Dir::new("waiting");
        std::fs::write(dir.file(), "old name\n").expect("the file to be there");
        let store = dir.store(Durability::Synced);

        store.stage("new name\n".to_owned());
        assert_eq!(store.read().as_deref(), Some("new name\n"));

        store.flush();
        assert_eq!(store.read().as_deref(), Some("new name\n"));
    }

    #[test]
    fn a_file_that_cannot_be_read_is_not_written_over() {
        let dir = Dir::new("unreadable");
        // Invalid UTF-8 rather than permissions, which the tests may well be
        // running as a user who has all of them.
        std::fs::write(dir.file(), [0xff, 0xfe, b'\n']).expect("the file to be there");

        let store = dir.store(Durability::Synced);
        assert!(store.read().is_none(), "nothing readable came back");
        assert!(store.sealed(), "and the store says why, for anyone asking");

        store.save_here("something else\n".to_owned());
        assert_eq!(
            std::fs::read(dir.file()).unwrap(),
            vec![0xff, 0xfe, b'\n'],
            "the file on disk was replaced although it was never read"
        );

        // Kept all the same. This is the hand-typed list: a station typed in
        // over a file that cannot be written is the only copy of it there is,
        // and dropping it meant the row was on screen until the pane was
        // opened again and then simply gone.
        assert_eq!(
            store.read().as_deref(),
            Some("something else\n"),
            "what was typed is still this session's, unwritable or not"
        );
    }

    #[test]
    fn a_store_with_nowhere_to_write_says_so() {
        // A machine with no config directory at all. Nothing is read, nothing
        // is written, and the one thing a caller can do about it is say so
        // before promising that an arrangement will still be there tomorrow.
        // Reported as a seal because it is the same sentence out loud: what
        // was typed in is this session's and no more than that.
        let store = Store::at(None, Durability::Rename);
        assert!(store.read().is_none());
        assert!(
            store.sealed(),
            "a store with nowhere to write can never keep anything"
        );

        // And what is typed into it is still the session's, exactly as over an
        // unreadable file.
        store.save_here("something\n".to_owned());
        assert_eq!(store.read().as_deref(), Some("something\n"));
    }

    #[test]
    fn a_write_that_could_not_be_made_says_so_too() {
        // The third way a file ends the run holding nothing this run put in
        // it: it was readable, there was somewhere to put it, and the write
        // itself did not land. Arranged as above, by putting a directory where
        // the temp file wants to be.
        let dir = Dir::new("said");
        let store = dir.store(Durability::Rename);
        assert!(!store.sealed(), "nothing has gone wrong yet");

        std::fs::create_dir_all(temp_name(&dir.file())).expect("the temp name taken");
        store.save_here("first\n".to_owned());
        assert!(
            store.sealed(),
            "a write that did not land is a file that will not hold what it \
             was given"
        );

        // And it is a report rather than a seal: the body is still staged and
        // the next write still tries, which is what makes the file readable
        // again at all.
        std::fs::remove_dir(temp_name(&dir.file())).expect("the temp name freed");
        store.save_here("second\n".to_owned());
        assert_eq!(std::fs::read_to_string(dir.file()).unwrap(), "second\n");
        assert!(
            !store.sealed(),
            "a write that landed answers for the ones before it"
        );
    }

    #[test]
    fn a_file_that_is_simply_missing_is_written_as_usual() {
        let dir = Dir::new("missing");
        let store = dir.store(Durability::Rename);

        assert!(store.read().is_none());
        store.save_here("first\n".to_owned());
        assert_eq!(std::fs::read_to_string(dir.file()).unwrap(), "first\n");
    }

    #[test]
    fn a_readable_file_unseals_the_store_again() {
        let dir = Dir::new("repaired");
        std::fs::write(dir.file(), [0xff]).expect("the file to be there");
        let store = dir.store(Durability::Rename);
        assert!(store.read().is_none());

        // Repaired by hand, as the log asked.
        std::fs::write(dir.file(), "mended\n").expect("the file to be there");
        assert_eq!(store.read().as_deref(), Some("mended\n"));
        store.save_here("written again\n".to_owned());
        assert_eq!(
            std::fs::read_to_string(dir.file()).unwrap(),
            "written again\n"
        );
    }

    impl Store {
        /// `save` without the runtime: staging and writing on this thread.
        ///
        /// `save` itself wants a `&'static self`, which a store made for one
        /// test is not.
        fn save_here(&self, body: String) {
            self.stage(body);
            self.flush();
        }
    }
}
