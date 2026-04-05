/* yang.h — C interface for the Yang macOS Swift app.
 *
 * Generated from crates/yang/src/ffi.rs — update both files together when the
 * FFI surface changes.
 */

#pragma once
#ifndef YANG_H
#define YANG_H

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Opaque handle ─────────────────────────────────────────────────────────── */

typedef struct YangSession YangSession;

/* ── Structs ───────────────────────────────────────────────────────────────── */

/**
 * Options for yang_connect().  All pointer fields are read only during the
 * yang_connect() call; they need not remain valid after it returns.
 */
typedef struct {
    /** Null-terminated server address, e.g. "192.168.1.50:9000". */
    const char *server_addr;
    /** Null-terminated display selector, or NULL for the first display. */
    const char *display_selector;
    /** Maximum bitrate cap in Mbps; 0 = unlimited. */
    uint32_t    max_bitrate_mbps;
    /** Minimum bitrate floor for adaptive control in Mbps; 0 = no floor. */
    uint32_t    min_bitrate_mbps;
    /** Maximum frames per second to request. */
    uint8_t     max_fps;
    /** Minimum frames per second the adaptive controller may target. */
    uint8_t     min_fps;
    /** Enable automatic bitrate/FPS adaptation. */
    bool        adaptive_streaming;
    /** Enable GPU optical-flow frame interpolation. */
    bool        interpolate;
} YangConnectOptions;

/** Live stream statistics delivered to the stats callback ~once per second. */
typedef struct {
    /** Presented frames in the last second (≈ fps). */
    float    fps;
    /** Estimated receive bitrate in Mbps (placeholder, currently always 0). */
    float    bitrate_mbps;
    /** Frames successfully decoded and presented. */
    uint32_t frames_decoded;
    /** Frames dropped by the render queue. */
    uint32_t frames_dropped;
    /** Frames lost unrecoverably (FEC could not reconstruct). */
    uint32_t unrecoverable_frames;
} YangStats;

/** Information about one display exported by the server. */
typedef struct {
    uint32_t index;
    uint32_t width;
    uint32_t height;
    /** Short display name (null-terminated). */
    char name[128];
    /** Stable machine-readable display id (null-terminated). */
    char id[128];
    /** Human-readable description (null-terminated). */
    char description[256];
} YangDisplayInfo;

/* ── Callback type ─────────────────────────────────────────────────────────── */

/**
 * Stats callback fired from a background thread ~1 Hz.
 * Dispatch to the main thread before touching any UI state.
 */
typedef void (*YangStatsCallback)(const YangStats *stats, void *userdata);

/* ── Functions ─────────────────────────────────────────────────────────────── */

/**
 * Connect to a Yin server and start streaming into ca_metal_layer.
 *
 * Blocks until the QUIC session is established (or fails).
 * Do NOT call from the macOS main thread.
 *
 * ca_metal_layer must be a valid CAMetalLayer* owned by the caller.
 * Rust configures the layer but never retains/releases the ObjC object.
 *
 * Returns a non-null YangSession* on success, NULL on failure.
 */
YangSession *yang_connect(
    const YangConnectOptions *opts,
    void                     *ca_metal_layer,
    YangStatsCallback         stats_cb,
    void                     *stats_userdata
);

/**
 * Signal shutdown and block until the render thread and network session stop.
 * Call yang_free() after this returns.
 */
void yang_disconnect(YangSession *session);

/** Deallocate the session. Must be called after yang_disconnect() returns. */
void yang_free(YangSession *session);

/**
 * Return the stream's pixel dimensions as negotiated with the server.
 * Safe to call from any thread after yang_connect() succeeds.
 * out_width and/or out_height may be NULL.
 */
void yang_stream_size(const YangSession *session, uint32_t *out_width, uint32_t *out_height);

/**
 * List displays available on the server.
 * Writes up to max_count entries into out.
 * Returns the number of displays written, or -1 on error.
 */
int yang_list_displays(
    const char       *server_addr,
    YangDisplayInfo  *out,
    int               max_count
);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* YANG_H */
