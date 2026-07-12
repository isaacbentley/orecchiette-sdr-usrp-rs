# Design: Ettus USRP Interface (orecchiette-sdr-usrp-rs)

This document outlines the architecture of the `orecchiette-sdr-usrp-rs` crate, providing integration with Ettus Research USRP devices (primarily the B210 and B205mini) via the `uhd` Rust bindings.

## 1. Introduction

The USRP B210 is the recommended SDR for high-fidelity detection in SDR detection applications due to its 12-bit ADC, excellent dynamic range, and USB 3.0 throughput. This crate wraps the `uhd` 0.3 crate (targeting UHD 4.x), implementing dynamic channel hopping, decimation strategies, and proactive buffer overrun mitigation.

## 2. System Architecture

The backend isolates the UHD streamer in a dedicated, high-priority capture thread.

```mermaid
graph TD
    A["start()"] --> B[Discover Device]
    B --> C[Select Master Clock & Decimation]
    C --> D[Configure Gain & Antenna]
    D --> E{Hop Loop}
    E --> F[set_rx_frequency]
    F --> G[get_rx_stream]
    G --> H[Stream 65,536-sample chunks]
    H --> I[Check for UHD Overflow]
    I --> J[Send IqPacket]
    J --> K{Dwell Met?}
    K -->|No| H
    K -->|Yes| L[Drop Streamer]
    L --> E
```

### The Borrow Checker Constraint
The `uhd` crate strictly models hardware state lifetimes. Both `Usrp::get_rx_stream()` and `Usrp::set_rx_frequency()` require `&mut self`. Because the `ReceiveStreamer` holds this mutable borrow, the orchestrator cannot tune the frequency while the streamer exists.
- **Resolution**: The capture thread deliberately drops and recreates the `ReceiveStreamer` on every channel hop. While this incurs a minor setup penalty, it safely satisfies Rust's memory model without dropping to unsafe C FFI `uhd-sys` calls.


## 3. Clock and Decimation Strategy

To maximize ADC resolution, the USRP operates best when the master clock rate is a high integer multiple of the requested sample rate.
- The backend automatically searches for the highest integer decimation factor (up to 4×) that keeps the master clock within the B210's 61.44 MHz ceiling.
- E.g., for a 15.36 MSPS request, the backend sets the master clock to 61.44 MHz and requests a decimation of 4. Each 4× oversampling step effectively buys ~1 bit of extra ADC resolution, lowering the noise floor.

## 4. Overrun Mitigation and Adaptive Tuning

Operating at high sample rates (e.g., 50 MSPS) over USB 3.0 risks saturating the host's USB controller, causing hardware buffer overflows (indicated by `O` characters in UHD stdout).

### Hardware Overrun Flags
When `streamer.receive()` returns `ReceiveErrorKind::Overflow`, the backend sets `IqPacket::overrun = true`. This metadata allows the downstream worker pool to gracefully discard corrupted DSP frames.

## 5. Configuration Validation and Failure Handling

- **Validate before opening hardware.** `validate_source_config()` rejects an
  empty `channels_hz` or a non-positive `sample_rate_hz` before `start()`
  calls `uhd::Usrp::find`/`open`/`set_rx_sample_rate` — a bad config fails
  fast without ever touching the device.
- **Bounded retry on a dead device.** If every channel in a sweep fails to
  tune/stream, the hop loop backs off 500ms and retries. After
  `MAX_CONSECUTIVE_SWEEP_FAILURES` (10) consecutive failed sweeps — meaning
  the device has been unresponsive for at least ~5 seconds — the capture
  thread gives up and exits instead of retrying forever, so `SdrHandle::wait()`
  eventually returns for a USRP that's been unplugged or wedged.
- **Buffer recycling.** Pooled IQ buffers are resized (not `unsafe`ly
  `set_len`'d) back to 65,536 samples before each `receive()` call. Buffers
  come back from the pool in two states — freshly `clear()`'d (len 0, via
  `PooledIqBuffer::drop`) or still at full length (the no-packet-sent
  put-back path) — and `Vec::resize` handles both without reallocating,
  since capacity always already covers 65,536.
