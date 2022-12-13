// This is the streaming state
//
// Data is streamed into a gstreamer source

use anyhow::{anyhow, Error, Result};
use crossbeam::utils::Backoff;
use log::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use neolink_core::bc_protocol::Stream;

use super::{CameraState, Shared};

use crate::rtsp::{
    abort::AbortHandle,
    gst::{GstOutputs, InputMode, PausedSources},
};

#[derive(Default)]
pub(crate) struct Streaming {
    handles: HashMap<Stream, JoinHandle<Result<(), Error>>>,
    outputs: HashMap<Stream, Arc<Mutex<GstOutputs>>>,
    abort_handle: AbortHandle,
}

impl CameraState for Streaming {
    fn setup(&mut self, shared: &Shared) -> Result<(), Error> {
        self.abort_handle.reset();
        // Create new gst outputs
        //
        // Otherwise use those already present
        if self.outputs.is_empty() {
            let paused_source = match shared.pause.mode.as_str() {
                "test" => PausedSources::TestSrc,
                "still" => PausedSources::Still,
                "black" => PausedSources::Black,
                "none" => PausedSources::None,
                _ => {
                    unreachable!()
                }
            };

            for stream in shared.streams.iter() {
                self.outputs.entry(*stream).or_insert_with_key(|stream| {
                    let paths = shared.get_paths(stream);
                    let mut output = shared
                        .rtsp
                        .add_stream(
                            paths
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<&str>>()
                                .as_slice(),
                            &shared.permitted_users,
                        )
                        .unwrap();
                    output.set_paused_source(paused_source);
                    Arc::new(Mutex::new(output))
                });
            }
        }

        // Start the streams on their own thread with a shared abort handle
        let camera = &shared.camera;
        let abort_handle = self.abort_handle.clone();

        for (stream, output) in &self.outputs {
            let stream_display_name = match stream {
                Stream::Main => "Main Stream (Clear)",
                Stream::Sub => "Sub Stream (Fluent)",
                Stream::Extern => "Extern Stream (Balanced)",
            };

            // Lock and setup output
            {
                let mut locked_output = output.lock().unwrap();
                locked_output.set_input_source(InputMode::Live)?;
            }

            info!(
                "{}: Starting video stream {}",
                &shared.name, stream_display_name
            );

            let arc_camera = camera.clone();
            let arc_abort_handle = abort_handle.clone();
            let output_thread = output.clone();

            let stream_thead = *stream;
            let handle = thread::spawn(move || {
                let backoff = Backoff::new();
                let stream_data = arc_camera.start_video(stream_thead, 0)?;

                while arc_abort_handle.is_live() {
                    let mut data = stream_data.get_data()?;
                    let mut locked_output = output_thread.lock().unwrap();
                    for datum in data.drain(..) {
                        locked_output.stream_recv(datum?)?;
                    }
                    backoff.spin();
                }
                Ok(())
            });

            self.handles.entry(*stream).or_insert_with(|| handle);
        }

        Ok(())
    }
    fn tear_down(&mut self, shared: &Shared) -> Result<(), Error> {
        self.abort_handle.abort();

        if !self.handles.is_empty() {
            for path in shared.get_all_paths().iter() {
                if let Err(e) = shared.rtsp.remove_stream(&[path]) {
                    return Err(anyhow!("Failed to shutdown RTSP Path {}: {:?}", path, e));
                }
            }

            for (stream, handle) in self.handles.drain() {
                match handle.join() {
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Err(anyhow!("Panicked while streaming {:?}", stream)),
                    Ok(Ok(_)) => {}
                }
            }
        }

        Ok(())
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        self.abort_handle.abort();

        for (stream, handle) in self.handles.drain() {
            if let Ok(Err(e)) = handle.join() {
                warn!("During drop: {:?} did not stop cleanly: {:?}", stream, e);
            } else {
                warn!("During drop: Panicked while streaming");
            }
        }
    }
}

impl Streaming {
    pub(crate) fn is_running(&self) -> bool {
        self.handles.iter().all(|(_, h)| !h.is_finished()) && self.abort_handle.is_live()
    }

    pub(crate) fn take_outputs(&mut self) -> Result<HashMap<Stream, GstOutputs>> {
        self.abort_handle.abort();
        for (stream, handle) in self.handles.drain() {
            match handle.join() {
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(anyhow!("Panicked while streaming {:?}", stream)),
                Ok(Ok(_)) => {}
            }
        }
        let mut result: HashMap<_, _> = Default::default();
        for (stream, arc_mutex_output) in self.outputs.drain() {
            let mutex_output =
                Arc::try_unwrap(arc_mutex_output).map_err(|_| anyhow!("Failed to unwrap ARC"))?;
            let output = mutex_output.into_inner()?;
            result.insert(stream, output);
        }
        Ok(result)
    }

    pub(crate) fn insert_outputs(&mut self, mut input: HashMap<Stream, GstOutputs>) -> Result<()> {
        self.outputs = input
            .drain()
            .map(|(s, o)| (s, Arc::new(Mutex::new(o))))
            .collect();
        Ok(())
    }

    pub(crate) fn client_connected(&self) -> bool {
        self.outputs
            .iter()
            .any(|(_, output)| output.lock().unwrap().is_connected())
    }

    pub(crate) fn can_pause(&self) -> bool {
        self.outputs
            .iter()
            .all(|(_, output)| output.lock().unwrap().has_last_iframe())
    }
}