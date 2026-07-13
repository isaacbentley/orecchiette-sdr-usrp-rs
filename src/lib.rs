#![doc = include_str!("../README.md")]
//! USRP-family SDR source for SDR applications.
//!
//! Implements [`orecchiette_sdr_source_rs::SdrSource`] for Ettus USRP devices
//! (tested on B210; B205mini should also work). Owns the device
//! handle, the channel-hop loop, and the IQ buffer allocation. The
//! orchestrator consumes [`IqPacket`]s through the receiver returned
//! in [`SdrHandle`].

use crossbeam::channel;
use num_complex::Complex32;
use orecchiette_sdr_source_rs::{
    DwellAdvice, DwellController, IqPacket, SdrError, SdrHandle, SdrSource, SourceConfig,
    freq_key_khz,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tracing::info;
use uhd::{StreamCommand, StreamCommandType, StreamTime, TuneRequest};

/// After this many consecutive full sweeps where every channel fails
/// to tune/stream (each sweep followed by a 500ms backoff sleep), give
/// up on the device rather than retrying forever — a USRP that's been
/// unplugged or wedged should surface as a terminal error instead of
/// spinning silently.
const MAX_CONSECUTIVE_SWEEP_FAILURES: u32 = 10;

/// Builder for a USRP source. Wrap in `Box::new(...)` and call
/// [`SdrSource::start`] from the orchestrator.
pub struct UsrpSource {
    /// UHD device args. Empty string lets UHD auto-discover. Common
    /// values: `"type=b200"`, `"serial=320XXXX"`.
    pub args: String,
    /// RX gain in dB. The B210 supports 0–76 dB; we default to a mid
    /// value (40 dB) that doesn't saturate on strong ambient ISM
    /// traffic.
    pub gain_db: f64,
    /// RX antenna port. B210 exposes `RX1` and `RX2`; B205mini has
    /// `RX2` only. Default `RX2`.
    pub antenna: String,
}

impl Default for UsrpSource {
    fn default() -> Self {
        Self {
            args: String::new(),
            gain_db: 40.0,
            antenna: "RX2".to_string(),
        }
    }
}

/// Reject an empty channel list or non-positive sample rate before any
/// hardware is touched. Kept as a standalone function so `start()` can
/// fail fast — and so this can be unit-tested — ahead of the
/// `uhd::Usrp::find`/`open`/`set_rx_sample_rate` calls it used to run
/// after.
fn validate_source_config(num_channels: usize, sample_rate: f64) -> Result<(), SdrError> {
    if num_channels == 0 {
        return Err(SdrError::BadConfig(
            "SourceConfig.channels_hz must not be empty".into(),
        ));
    }
    if sample_rate <= 0.0 {
        return Err(SdrError::BadConfig(
            "SourceConfig.sample_rate_hz must be > 0".into(),
        ));
    }
    Ok(())
}

/// Optimal master clock selection: highest integer decimation (up to
/// 4×) within the 61.44 MHz limit. Each 4× oversampling step yields
/// ~1 additional bit of ENOB at the cost of more FPGA work, which the
/// B210 can deliver up to its ceiling.
fn select_master_clock(sample_rate: f64) -> (f64, u32) {
    if sample_rate * 4.0 <= 61.44e6 {
        (sample_rate * 4.0, 4)
    } else if sample_rate * 2.0 <= 61.44e6 {
        (sample_rate * 2.0, 2)
    } else {
        (sample_rate, 1)
    }
}

impl SdrSource for UsrpSource {
    fn start(
        self: Box<Self>,
        config: SourceConfig,
        advice: Arc<dyn DwellAdvice>,
    ) -> Result<SdrHandle, SdrError> {
        let sample_rate = config.sample_rate_hz;
        let channels_hz = config.channels_hz.clone();
        let num_channels = channels_hz.len();
        validate_source_config(num_channels, sample_rate)?;

        let (master_clock, decimation) = select_master_clock(sample_rate);

        let bit_gain = (decimation as f32).log2() * 0.5;
        let total_bits = 12.0 + bit_gain;

        info!(
            "[usrp] Configuring hardware: Rate={:.2} MSPS | Clock={:.2} MHz | Decimation={}x",
            sample_rate / 1e6,
            master_clock / 1e6,
            decimation
        );
        info!(
            "[usrp] Signal Quality: Effective ADC Resolution = {:.2} bits (+{:.2} bits gain)",
            total_bits, bit_gain
        );

        let dev_args = if self.args.is_empty() {
            format!("master_clock_rate={}", master_clock)
        } else {
            format!("{},master_clock_rate={}", self.args, master_clock)
        };

        let devices =
            uhd::Usrp::find("").map_err(|e| SdrError::Io(format!("USRP find failed: {e}")))?;
        if devices.is_empty() {
            return Err(SdrError::NotFound(
                "No USRP devices found. Ensure the USRP is connected and powered on.".into(),
            ));
        }

        info!("[usrp] Opening device with args: \"{}\"", dev_args);
        let mut usrp = uhd::Usrp::open(&dev_args)
            .map_err(|e| SdrError::Io(format!("USRP open failed: {e}")))?;

        usrp.set_rx_sample_rate(sample_rate, 0)
            .map_err(|e| SdrError::BadConfig(format!("set_rx_sample_rate({sample_rate}): {e}")))?;
        usrp.set_rx_gain(self.gain_db, 0, "")
            .map_err(|e| SdrError::BadConfig(format!("set_rx_gain({}): {e}", self.gain_db)))?;
        usrp.set_rx_antenna(&self.antenna, 0)
            .map_err(|e| SdrError::BadConfig(format!("set_rx_antenna({}): {e}", self.antenna)))?;

        let dwell_controller = DwellController {
            min: config.dwell_min,
            max: config.dwell_max,
            extension: config.dwell_extension,
        };

        if dwell_controller.is_adaptive() {
            info!(
                "[usrp] Starting scan: {} channels, adaptive dwell {}–{}ms (+{}ms per detection)",
                num_channels,
                config.dwell_min.as_millis(),
                config.dwell_max.as_millis(),
                config.dwell_extension.as_millis()
            );
        } else {
            info!(
                "[usrp] Starting scan: {} channels, fixed {}ms dwell per channel",
                num_channels,
                config.dwell_min.as_millis()
            );
        }

        let (tx, receiver) = channel::bounded::<IqPacket>(1024);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = stop_flag.clone();
        let advice_thread = advice;
        let sample_rate_f32 = sample_rate as f32;

        let capture_thread = thread::spawn(move || {
            let panic_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let hop_result: Result<(), anyhow::Error> = {
                    // uhd 0.3.0's `Usrp::get_rx_stream` and `Usrp::set_rx_frequency` both take
                    // `&mut self`, and `ReceiveStreamer<'_>` holds the mutable borrow for its
                    // lifetime. We therefore have to recreate the streamer per hop. Eliminating
                    // that overhead requires either upgrading/forking the uhd binding or dropping
                    // to uhd-sys and managing the C handle ourselves. For now we lift everything
                    // that *can* live outside the loop and accept the per-hop streamer teardown.
                    let stream_args = uhd::StreamArgs::builder()
                        .wire_format("sc16".to_string())
                        .args("num_recv_frames=1024".to_string())
                        .build();

                    // Pre-allocate the vector recycling pool
                    let (pool_tx, pool_rx) = channel::bounded::<Vec<Complex32>>(1024);
                    for _ in 0..1024 {
                        let _ = pool_tx.send(vec![Complex32::new(0.0, 0.0); 65536]);
                    }

                    let mut last_report = Instant::now();
                    let mut channel_switches = 0;
                    let mut channel_idx = 0;
                    let mut consecutive_failures = 0;
                    let mut consecutive_sweep_failures = 0;
                    let num_channels = channels_hz.len();

                    'outer: loop {
                        if stop_flag_thread.load(Ordering::SeqCst) {
                            break;
                        }

                        if consecutive_failures >= num_channels {
                            consecutive_sweep_failures += 1;
                            if consecutive_sweep_failures >= MAX_CONSECUTIVE_SWEEP_FAILURES {
                                tracing::error!(
                                    "[usrp] All channels failed to tune for {} consecutive sweeps. Giving up — is the USRP still connected?",
                                    consecutive_sweep_failures
                                );
                                break 'outer;
                            }
                            tracing::warn!(
                                "[usrp] All channels failed to tune consecutively. Sleeping for 500ms before retrying."
                            );
                            thread::sleep(Duration::from_millis(500));
                            consecutive_failures = 0;
                        }

                        let current_freq_hz = channels_hz[channel_idx];
                        let freq_key = freq_key_khz(current_freq_hz);

                        if let Err(e) =
                            usrp.set_rx_frequency(&TuneRequest::with_frequency(current_freq_hz), 0)
                        {
                            tracing::warn!(
                                "[usrp] Failed to set frequency to {} Hz: {:?}. Skipping channel.",
                                current_freq_hz,
                                e
                            );
                            consecutive_failures += 1;
                            channel_idx = (channel_idx + 1) % num_channels;
                            continue;
                        }

                        let mut rx_stream = match usrp.get_rx_stream(&stream_args) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!(
                                    "[usrp] Failed to get RX stream for {} Hz: {:?}. Skipping channel.",
                                    current_freq_hz,
                                    e
                                );
                                consecutive_failures += 1;
                                channel_idx = (channel_idx + 1) % num_channels;
                                continue;
                            }
                        };

                        if let Err(e) = rx_stream.send_command(&StreamCommand {
                            command_type: StreamCommandType::StartContinuous,
                            time: StreamTime::Now,
                        }) {
                            tracing::warn!(
                                "[usrp] Failed to send start command for {} Hz: {:?}. Skipping channel.",
                                current_freq_hz,
                                e
                            );
                            consecutive_failures += 1;
                            channel_idx = (channel_idx + 1) % num_channels;
                            continue;
                        }

                        // Reset consecutive failures on successful tune/start
                        consecutive_failures = 0;
                        consecutive_sweep_failures = 0;

                        let dwell_start = Instant::now();
                        loop {
                            if stop_flag_thread.load(Ordering::SeqCst) {
                                let _ = rx_stream.send_command(&StreamCommand {
                                    command_type: StreamCommandType::StopContinuous,
                                    time: StreamTime::Now,
                                });
                                drop(rx_stream);
                                break 'outer;
                            }
                            let now_loop = Instant::now();
                            // With a single channel there is nowhere to hop, so
                            // never end the dwell on the deadline — stream
                            // continuously. Otherwise a single-channel caller
                            // with a short `dwell_min` (e.g. a wideband
                            // channelizer) would drop and recreate the RX
                            // streamer every dwell period, punching periodic gaps
                            // into an otherwise continuous stream. The dwell
                            // deadline only gates hopping, which needs
                            // `num_channels > 1`.
                            if num_channels > 1 {
                                let latest_signal = advice_thread.latest_signal_at(freq_key);
                                let deadline =
                                    dwell_controller.deadline(dwell_start, latest_signal);
                                if now_loop >= deadline {
                                    break;
                                }
                            }

                            // Borrow an empty buffer from the pool (or allocate if heavily backed up)
                            let mut raw_buffer = Some(
                                pool_rx
                                    .try_recv()
                                    .unwrap_or_else(|_| vec![Complex32::new(0.0, 0.0); 65536]),
                            );
                            {
                                // Present a full-length 65536-element buffer to
                                // `receive`. Buffers recycled through
                                // `PooledIqBuffer::drop` come back at len 0 (but
                                // capacity 65536), so this resize both restores
                                // the length UHD expects and re-zeroes memory
                                // that `clear()` had logically invalidated — no
                                // reallocation occurs since capacity already
                                // covers it.
                                let buf = raw_buffer.as_mut().unwrap();
                                buf.resize(65536, Complex32::new(0.0, 0.0));
                            }

                            let mut put_back = true;
                            let mut buffers = [&mut raw_buffer.as_mut().unwrap()[..]];
                            match rx_stream.receive(&mut buffers, 0.05, false) {
                                Ok(meta) => {
                                    let n = meta.samples().min(raw_buffer.as_ref().unwrap().len());
                                    if n > 0 {
                                        let is_overrun = if let Some(err) = meta.last_error() {
                                            err.to_string().contains("Overflow")
                                        } else {
                                            false
                                        };

                                        let mut buf = raw_buffer.take().unwrap();
                                        // Truncate the vector to exactly n samples (capacity is maintained)
                                        buf.truncate(n);

                                        let pkt = IqPacket {
                                            samples: orecchiette_sdr_source_rs::PooledIqBuffer::new_pooled(
                                                buf,
                                                pool_tx.clone(),
                                            ),
                                            center_frequency_hz: current_freq_hz,
                                            sample_rate_hz: sample_rate_f32,
                                            overrun: is_overrun,
                                        };
                                        if tx.send(pkt).is_err() {
                                            // Receiver dropped — wind down.
                                            let _ = rx_stream.send_command(&StreamCommand {
                                                command_type: StreamCommandType::StopContinuous,
                                                time: StreamTime::Now,
                                            });
                                            drop(rx_stream);
                                            break 'outer;
                                        }
                                        put_back = false;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "[usrp] Receive error on frequency {} Hz: {:?}",
                                        current_freq_hz,
                                        e
                                    );
                                }
                            }

                            if put_back && let Some(buf) = raw_buffer {
                                let _ = pool_tx.send(buf);
                            }

                            let elapsed = now_loop.duration_since(last_report);
                            if elapsed >= Duration::from_secs(60) {
                                let rate = channel_switches as f32 / elapsed.as_secs_f32();
                                info!(
                                    "[usrp] Scanning speed: {:.1} ch/s | Pool size: {} channels",
                                    rate, num_channels
                                );
                                channel_switches = 0;
                                last_report = now_loop;
                            }
                        }

                        let _ = rx_stream.send_command(&StreamCommand {
                            command_type: StreamCommandType::StopContinuous,
                            time: StreamTime::Now,
                        });
                        drop(rx_stream);

                        channel_idx = (channel_idx + 1) % num_channels;
                        channel_switches += 1;
                    }
                    Ok(())
                };
                if let Err(e) = hop_result {
                    tracing::error!("[usrp] Capture thread failed: {:?}", e);
                }
            }));
            if let Err(e) = panic_res {
                tracing::error!("[usrp] Capture thread panicked: {:?}", e);
            }
        });

        let stop_flag_for_stop = stop_flag.clone();
        let stop = Box::new(move || {
            stop_flag_for_stop.store(true, Ordering::SeqCst);
        });
        let wait = Box::new(move || {
            if let Err(e) = capture_thread.join() {
                tracing::error!("[usrp] capture thread join failed: {:?}", e);
            }
        });

        Ok(SdrHandle {
            receiver,
            stop,
            wait,
        })
    }
}

/// Probe the connected USRP for its maximum supported RX sample rate.
///
/// Opens the device (with optional `usrp_args`), queries
/// `get_rx_sample_rates(0)` → `MetaRange`, and returns the highest
/// `stop()` value across all sub-ranges. The device handle is dropped
/// immediately — no streaming is started.
///
/// For B210 this returns 61.44 MSPS, B205mini 56 MSPS, N310
/// 122.88 MSPS, etc.
pub fn query_max_rx_rate(usrp_args: &str) -> Result<f64, SdrError> {
    let usrp =
        uhd::Usrp::open(usrp_args).map_err(|e| SdrError::Io(format!("USRP open failed: {e}")))?;
    let meta_range = usrp
        .get_rx_sample_rates(0)
        .map_err(|e| SdrError::Io(format!("get_rx_sample_rates failed: {e}")))?;
    let max_rate = meta_range
        .stop()
        .map_err(|e| SdrError::Io(format!("MetaRange::stop() failed: {e}")))?;
    if max_rate <= 0.0 {
        return Err(SdrError::BadConfig(
            "USRP returned no valid RX sample rates".into(),
        ));
    }
    Ok(max_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_source_config_rejects_empty_channels() {
        let err = validate_source_config(0, 2_000_000.0).unwrap_err();
        assert!(matches!(err, SdrError::BadConfig(_)));
    }

    #[test]
    fn validate_source_config_rejects_non_positive_sample_rate() {
        assert!(validate_source_config(1, 0.0).is_err());
        assert!(validate_source_config(1, -1.0).is_err());
    }

    #[test]
    fn validate_source_config_accepts_sane_input() {
        assert!(validate_source_config(4, 2_000_000.0).is_ok());
    }

    #[test]
    fn select_master_clock_prefers_4x_within_ceiling() {
        // 10 MSPS * 4 = 40 MHz, under the 61.44 MHz ceiling.
        let (clock, decimation) = select_master_clock(10_000_000.0);
        assert_eq!(decimation, 4);
        assert!((clock - 40_000_000.0).abs() < 1.0);
    }

    #[test]
    fn select_master_clock_falls_back_to_2x() {
        // 20 MSPS * 4 = 80 MHz (over ceiling); * 2 = 40 MHz (under).
        let (clock, decimation) = select_master_clock(20_000_000.0);
        assert_eq!(decimation, 2);
        assert!((clock - 40_000_000.0).abs() < 1.0);
    }

    #[test]
    fn select_master_clock_falls_back_to_1x_at_the_ceiling() {
        // 61.44 MSPS: neither *4 nor *2 fit, so no decimation.
        let (clock, decimation) = select_master_clock(61.44e6);
        assert_eq!(decimation, 1);
        assert!((clock - 61.44e6).abs() < 1.0);
    }

    #[test]
    fn recycled_buffer_resize_matches_pooled_iq_buffer_drop_semantics() {
        // `PooledIqBuffer::drop` (orecchiette-sdr-source-rs) does
        // `vec.clear()` before recycling — len 0, capacity retained.
        // The capture loop's `buf.resize(65536, ..)` must restore a
        // full-length buffer without reallocating.
        let mut cleared = Vec::with_capacity(65536);
        cleared.resize(65536, Complex32::new(1.0, 1.0));
        cleared.clear();
        let cap_before = cleared.capacity();

        cleared.resize(65536, Complex32::new(0.0, 0.0));
        assert_eq!(cleared.len(), 65536);
        assert_eq!(cleared.capacity(), cap_before, "resize must not reallocate");
        assert_eq!(cleared[0], Complex32::new(0.0, 0.0));

        // A buffer put back directly at full length (the no-packet-sent
        // path) is already len 65536 — resize must be a no-op that
        // preserves its (stale but initialized) contents.
        let mut full = vec![Complex32::new(3.0, 4.0); 65536];
        full.resize(65536, Complex32::new(0.0, 0.0));
        assert_eq!(full[0], Complex32::new(3.0, 4.0));
    }
}
