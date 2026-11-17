// Unit tests for the extension-side preset bookkeeping. Runs under `gjs -m`
// (see `make test-js`), so it needs no compositor and no D-Bus.

import {PresetQueue} from '../../platform/gnome-extension/sizer@sizerlinux.dev/preset_state.js';

let failures = 0;

function check(name, condition, detail = '') {
    if (condition) {
        print(`PASS ${name}`);
        return;
    }
    failures += 1;
    print(`FAIL ${name}${detail ? ': ' + detail : ''}`);
}

// A newer request on the same window invalidates the older one.
{
    const queue = new PresetQueue();
    const first = queue.begin('win');
    check('first request is current', queue.isCurrent(first));
    const second = queue.begin('win');
    check('newer request is current', queue.isCurrent(second));
    check('older request is stale', !queue.isCurrent(first));
    check('generations increase', second.generation > first.generation);
}

// Generations are per window: work on one window must not invalidate another.
{
    const queue = new PresetQueue();
    const left = queue.begin('left');
    const right = queue.begin('right');
    check('both windows are current', queue.isCurrent(left) && queue.isCurrent(right));
    const leftAgain = queue.begin('left');
    check('newest request on the first window is current', queue.isCurrent(leftAgain));
    check('older request on the first window is stale', !queue.isCurrent(left));
    check('the other window is unaffected', queue.isCurrent(right));
}

// A newer request leaves the older wait in place: that wait is what keeps the
// follow-up queued behind the compositor instead of writing too early. The
// stale request reports itself when its wait ends.
{
    const queue = new PresetQueue();
    const stale = queue.begin('w1');
    queue.defer(stale, () => {});
    const fresh = queue.begin('w1');
    check('superseded request is stale', !queue.isCurrent(stale));
    check('its wait stays recorded for the follow-up', queue.waiting('w1'));
    check('the new request is current', queue.isCurrent(fresh));
}

// A request on another window leaves this window's wait alone.
{
    const queue = new PresetQueue();
    let cancelled = 0;
    const mine = queue.begin('w1');
    queue.defer(mine, () => { cancelled += 1; });
    queue.begin('w2');
    check('other window does not cancel this wait', cancelled === 0 && queue.waiting('w1'));
    check('the waiting request is still current', queue.isCurrent(mine));
}

// Waiting is per window: another window's request does not serialise this one.
{
    const queue = new PresetQueue();
    const mine = queue.begin('w1');
    check('nothing waits initially', !queue.waiting('w1'));
    queue.defer(mine, () => {});
    check('deferred window waits', queue.waiting('w1'));
    const other = queue.begin('w2');
    check('other window does not wait', !queue.waiting('w2'));
    queue.defer(other, () => {});
    check('both windows wait', queue.waiting('w1') && queue.waiting('w2'));
    queue.settled(mine);
    check('settled request stops waiting', !queue.waiting('w1'));
    check('the other request still waits', queue.waiting('w2'));
    queue.settled(mine);
    check('settling twice is harmless', queue.waiting('w2'));
}

// A stale request stays stale even after the newer one settles.
{
    const queue = new PresetQueue();
    const first = queue.begin('win');
    const second = queue.begin('win');
    queue.settled(second);
    check('superseded request does not become current again', !queue.isCurrent(first));
}

// Teardown cancels every pending wait exactly once.
{
    const queue = new PresetQueue();
    const cancelled = [];
    queue.defer(queue.begin('w1'), () => cancelled.push('w1'));
    queue.defer(queue.begin('w2'), () => cancelled.push('w2'));
    queue.cancelAll();
    check('cancelAll runs every cancel', cancelled.length === 2,
          `cancelled ${cancelled.length}`);
    check('cancelAll clears the queue', !queue.waiting('w1') && !queue.waiting('w2'));
    queue.cancelAll();
    check('cancelAll is idempotent', cancelled.length === 2, `cancelled ${cancelled.length}`);
}

print(`SUMMARY fail=${failures}`);
if (failures)
    imports.system.exit(1);
