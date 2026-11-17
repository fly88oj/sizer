'use strict';

/// Bookkeeping for preset requests, with no shell dependency so it can be tested
/// on its own.
///
/// Two questions the extension has to answer while a write waits for the
/// compositor:
///
///  - is this callback still the newest request *for its window*? A newer request
///    on the same window invalidates the older ones, so a superseded callback
///    must not write. Generations are per window: a preset applied to window B
///    must not make window A's pending write stale, or the user sees "sometimes
///    a key press does nothing".
///  - does this window already have a write waiting? A second preset picked while
///    the first one's unmaximize is in flight must queue behind it instead of
///    writing against a frame offset the compositor has not settled yet.
export class PresetQueue {
    constructor() {
        this._generations = new Map();
        this._waiting = [];
    }

    /// Start a request on `window`; every earlier request on that window becomes
    /// stale. The wait an older request still has is left in place: it is what
    /// keeps a follow-up request on the same window queued behind the compositor
    /// instead of writing against an offset that has not settled. When that wait
    /// ends, the stale request reports itself as superseded.
    begin(window) {
        const generation = (this._generations.get(window) ?? 0) + 1;
        this._generations.set(window, generation);
        return {window, generation};
    }

    /// Whether `request` is still the newest one for its window.
    isCurrent(request) {
        return this._generations.get(request.window) === request.generation;
    }

    /// Record that `request` waits for the compositor; `cancel` stops the wait.
    ///
    /// `cancel` is called when the extension is torn down, where there is nobody
    /// left to report to. A wait the extension itself ends (window destroyed,
    /// request superseded) is reported by the extension, not by this queue.
    defer(request, cancel) {
        const entry = {request, cancel};
        this._waiting.push(entry);
        return entry;
    }

    /// Record that `request` no longer waits.
    settled(request) {
        this._waiting = this._waiting.filter(entry => entry.request !== request);
    }

    /// Whether some write is still waiting on `window`.
    waiting(window) {
        return this._waiting.some(entry => entry.request.window === window);
    }

    /// Stop every pending wait, e.g. when the extension is disabled.
    cancelAll() {
        const entries = this._waiting;
        this._waiting = [];
        for (const entry of entries)
            entry.cancel();
    }
}
