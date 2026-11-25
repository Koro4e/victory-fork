# GPU-driven frame (`VICTORY_FRAME_REPLAY=1`)

Branch `gpu-frame`. `main` is untouched; every change sits behind the switch,
which is off by default, so a run without it behaves exactly as today. The
translation fingerprint picks the switch up, so cached translations of the
plain path stay valid.

## The problem

A frame is 747+ draws and dozens of lists the host records and submits one
by one. Each draw pays for descriptor binding, root signatures, barriers and
list bookkeeping on the CPU, and each list pays for a submission. The console
pays none of that: its command processor walks its own command buffers. As
the world grows (more draws, more lists), this tax grows with it, and it is
the wall between 3 fps and 60.

## The idea

Record the title's frame once into a GPU-resident structure, then replay it
on the card with CPU work that does not grow with the draw count:

1. **Capture**: while a frame's lists are recorded the plain way, also log
   the draws and dispatches (pipeline, arguments, index buffer, instance
   count) into a GPU buffer, in list order.
2. **Replay**: one (or few) host dispatches walk that buffer on the card —
   a `ExecuteIndirect` chain per command kind, or one indirection that fans
   out — so the card runs the whole frame from the buffer.
3. **State**: render targets, descriptor heaps and resource bindings stay
   as they are at first; only the draw/dispatch submission is replayed.

## Slices (each lands behind the switch, each measured)

1. **Count and classify** (no behaviour change): log per frame how many
   draws/dispatches are replay-eligible (graphics with static root
   arguments, compute with constant root arguments) versus not (dynamic
   descriptor tables, root constants that change per draw). Decides the
   shape of slice 2. Measured on the 3060 in the world.
2. **Replay compute first**: compute dispatches are state-light (no render
   targets, no index buffers). Replay them through one `ExecuteIndirect`
   over an argument buffer filled by the capture. Measure.
3. **Replay graphics draws**: the same over draws; per-draw vertex/index
   buffer and descriptor table pointers live in the argument buffer.
4. **Cut the recording**: once replay works, the per-draw host recording
   itself is replaced by the capture writes (the title's command stream is
   translated to buffer writes, not host calls).

## Rollback

- `git checkout main` — the branch is never merged until a slice proves
  itself in the world.
- No flag, no effect: the plain path's code is not modified, only added to.
- Builds go to their own target folder; the working exe is not replaced.

## What this does not fix

Shader translation quality (the top compute programs' divergence), the
copy-back relay (that is the lazy-memory work), and ray-tracing structure
building. It removes the per-draw and per-list CPU tax only.
