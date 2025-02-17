//! Simple daemon
//!
//! This daemon will start web server, track new packages and build them

use crate::{
    utils::{queue_builder, report_error},
    Context, RustwideBuilder,
};
use anyhow::{anyhow, Context as _, Error};
use log::{debug, info};
use std::thread;
use std::time::{Duration, Instant};

fn start_registry_watcher(context: &dyn Context) -> Result<(), Error> {
    let build_queue = context.build_queue()?;
    let config = context.config()?;
    let index = context.index()?;

    thread::Builder::new()
        .name("registry index reader".to_string())
        .spawn(move || {
            // space this out to prevent it from clashing against the queue-builder thread on launch
            thread::sleep(Duration::from_secs(30));

            let mut last_gc = Instant::now();
            loop {
                if build_queue.is_locked() {
                    debug!("Lock file exists, skipping checking new crates");
                } else {
                    debug!("Checking new crates");
                    match build_queue
                        .get_new_crates(&index)
                        .context("Failed to get new crates")
                    {
                        Ok(n) => debug!("{} crates added to queue", n),
                        Err(e) => report_error(&e),
                    }
                }

                if last_gc.elapsed().as_secs() >= config.registry_gc_interval {
                    index.run_git_gc();
                    last_gc = Instant::now();
                }
                thread::sleep(Duration::from_secs(60));
            }
        })?;

    Ok(())
}

pub fn start_daemon(context: &dyn Context, enable_registry_watcher: bool) -> Result<(), Error> {
    // Start the web server before doing anything more expensive
    // Please check with an administrator before changing this (see #1172 for context).
    info!("Starting web server");
    let server = crate::Server::start(None, context)?;
    let server_thread = thread::spawn(|| drop(server));

    if enable_registry_watcher {
        // check new crates every minute
        start_registry_watcher(context)?;
    }

    // build new crates every minute
    let build_queue = context.build_queue()?;
    let rustwide_builder = RustwideBuilder::init(context)?;
    thread::Builder::new()
        .name("build queue reader".to_string())
        .spawn(move || {
            queue_builder(rustwide_builder, build_queue).unwrap();
        })
        .unwrap();

    // This call will still skip github repositories updates and continue if no token is provided
    // (gitlab doesn't require to have a token). The only time this can return an error is when
    // creating a pool or if config fails, which shouldn't happen here because this is run right at
    // startup.
    let updater = context.repository_stats_updater()?;
    cron(
        "repositories stats updater",
        Duration::from_secs(60 * 60),
        move || {
            updater.update_all_crates()?;
            Ok(())
        },
    )?;

    // Never returns; `server` blocks indefinitely when dropped
    // NOTE: if a anyhow occurred earlier in `start_daemon`, the server will _not_ be joined -
    // instead it will get killed when the process exits.
    server_thread
        .join()
        .map_err(|_| anyhow!("web server panicked"))
}

pub(crate) fn cron<F>(name: &'static str, interval: Duration, exec: F) -> Result<(), Error>
where
    F: Fn() -> Result<(), Error> + Send + 'static,
{
    thread::Builder::new()
        .name(name.into())
        .spawn(move || loop {
            thread::sleep(interval);
            if let Err(err) =
                exec().with_context(|| format!("failed to run scheduled task '{}'", name))
            {
                report_error(&err);
            }
        })?;
    Ok(())
}
