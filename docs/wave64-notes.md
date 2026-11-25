# Wave64 → wave32: состояние работы (сессия 24.11, вечер)

## Контекст
Хост (RTX 3060) отказывает пайплайнам с `WaveSize(64)` — максимум 32.
Из-за этого 4 compute-программы билдера RT-структур не создаются →
`builder_runs()` = all-or-nothing → весь билдер off → 718 дропов/кадр →
нет непрямого света (картинка тёмная).

Отказанные (хэши шейдеров из лога, сами дампы не публикуются):
`02384a60fda6f2d0` (group=128), `2cc4c197ee29e291`,
`369cbe906ea72764`, `2da4beb386509b77` (group=64).
Причина отказа (debug layer): «Shader wants wave lane count of 64 but
maximum wave lane count for device is 32».

## Что уже есть в форке (fxceless)
- Халвинг 64→32 в `crates/victory-shader/src/emit/lower.rs` (~1290–1450):
  консольная волна 64 = две host-волны 32, `wave_base` перемаркирует
  thread id, `exchange`-dwords в LDS для кросс-половинных операций
  (`v_mbcnt`, `v_readlane`, маски-как-данные).
- Ограничения халвинга: `crosses` (кросс-половинные операции) и
  разделяемый LDS в group>32 (`lds_ok`/`lds_meetings`).
- `lds_meetings` ищет точки встречи всех лайнов (начало/выход region);
  возвращает None, если в одном lane-branch region есть И чтение И запись
  LDS — барьер нельзя поставить в ветвящемся коде.

## Что сделано нами в этой сессии
1. **Пробник `lds_dump`** (`crates/victory-shader/examples/lds_dump.rs`):
   декодирует .cdxil, печатает все ds_* с полями и 12 инструкций перед
   ними. Запуск: `lds_dump.exe <file.cdxil>`.
2. **Taint-анализ `lds_lane_private(p)`** в `lower.rs` (перед
   `lds_meetings`): адрес каждого ds-доступа трассируется по SSA до
   источников. Taint: 1 = per-lane (entry-VGPR, т.е. thread id), 2 =
   uniform (SGPR/константа), 4 = unknown (память/LDS/vcc-мимо).
   Программа проходит, если КАЖДЫЙ адрес содержит per-lane источник.
   Such LDS needs no ordering between host waves → халвинг без барьеров.
3. **Врезка в решение** (~1404): `lds_private` разрешает и быстрый путь
   (`!crosses && (... || lds_private)`), и `lds_ok`, и убирает
   `split_lds`. Лог: `WAVE64-LDS-PRIVATE group=… lds=…`.
4. Проверено end-to-end: `03e3e26783a5799b` (group=64, lds=128) →
   probe.dxil с `WaveSize=32` (dxc -dumpbin). Раньше остался бы 64.

## Границы (честно)
- 4 отказанных программы билдера НЕ проходят через `lds_lane_private` —
  у них реально разделяемый LDS: например `2cc4c197`: block 0 — lane 0
  (guard `v1|v0 == 0`) пишет 2 dword по константному адресу 0, дальше
  все читают; доступы в divergent-регионе (blocks 1–35) → `lds_meetings`
  = None. Это не баг анализа — так и есть.
- Полностью sound-решение для таких — «секвенциальные сегменты»: одна
  host-волна 32 прогоняет 64 консольных потока двумя проходами через
  тело программы (LDS общий, т.к. один dispatch), барьеры = границы
  сегментов (чередование половин). D3D12-совместимо (LDS живёт в
  dispatch). Это большой кусок эмиттера; автор форка активно работает
  в этой области (коммиты 90228a7, e9765a7 от 24.11).
- `VICTORY_WAVE_LDS_ASSUME=1` — форс-флаг «считать всё lane-private»
  (для экспериментов; карантин `VICTORY_BVH_QUARANTINE=1` удержит мусор
  от записи в память тайтла).

## Скан всех дамп (в процессе)
Прогон пробника по всем дампам с `VICTORY_WAVE_WHY=1`: сколько `ok ->`
(wave32), сколько `WAVE64-WHY`, сколько `LDS-PRIVATE`.
Итог на 24.11: 1163 compute-программы, 1154 wave32-совместимы,
90 остались wave64, 37 прошли через `lds_lane_private`.

## Прочее из сессии (кратко)
- `VICTORY_TEXTURE_NO_WAIT=1`: текстуры 312–545 → 12–23 мс.
- Card-буферы (`VICTORY_CARD_BUFFERS_MIN_MB=8, MB=1536`): heap-pass
  262→15–24 мс, draws 1906→4833, fps 1.8→2.5–4.
- GPU-стены (run4-diag): indirect compute f22a76fc 22.4 мс/кадр
  (145 сэмплов), dispatch 4a05640e 20.2 (culling), mesh ef540905+pixel
  94d28115 14.6, compute cfd83392 10.5. Серебряной пули нет.
- 4 отказанных compute ВАЛИДНЫ по DXIL (validate_dump.exe) — дело только
  в WaveSize. 8 графических невалидны (сигнатуры ISG1/OSG1/PSV0 в
  `emit/container.rs`) — отдельная работа.
- Сейвы пишутся и перезапуски прогресс не теряют.

## RT-light builder blocker (f133e35efed2a48a, 0x1eb4) — analysis 2026-11-25

`wave_why` reproduces: `translate failed: GFX10 code at 0x1eb4: a lane mask exchanged
where not every lane runs is not supported`.

Native context (dis_at 0x1eb4): `v_cmpx_lt_f32` (EXEC-writing compare) at 0x1e74 →
`s_cbranch_execz` → scalar loads → `s_mov_b64`/`s_and_b64` building a mask →
`s_cbranch_vccz`. The vote the translator refuses is a ballot in a block its
convergence model does not consider all-lanes.

Root cause hypothesis: the native code restores EXEC (`s_or_b64 exec, exec, -1`-style)
before voting; the translator's `s_or_b64` handler (lower.rs ~5483) treats EXEC as a
plain scalar write and the convergence model never learns the point is converged.
The ballot check (lower.rs ~3508, `all_lanes(b)`) then refuses.

Fix direction: detect EXEC-destination scalar ops that write all-ones (s_mov_b64 exec,
s_or_b64 exec with ones, s_andn2_b64) and mark the position converged so subsequent
ballots pass `all_lanes`. Needs a "converged here" marker in the model, not a block
flag — multi-hour task. Escape hatch in the check: `exchange.1 == exact` skips it.

## Run20+: TARGET_SCALE (nuclear) — 2026-11-25

The title renders EVERYTHING at 3840x2160 (TARGET_SIZES: only 3840x2160 x90-155
a frame; no DRS, no smaller internal targets). `VICTORY_TARGET_SCALE=0.5`:

- `resource::scale_target_desc` halves host textures with RENDER_TARGET or
  DEPTH_STENCIL flags (samples==1, 2D, >16px); `twin_texture` halves the depth
  twins to match (fixed after the first build shipped without it).
- `draw::scale_viewports`/`scale_scissors` scale the title's viewports and
  scissors by the same fraction; the rasterizer then fills the smaller targets
  with the whole scene.
- The present path needs no change: the present texture is a target too, the
  swap chain is sized from the host copy's desc (1920x1080 confirmed in the log).

Menu in run20: 23.7-28.5 fps (42/35 ms frames), GPU 1.8-9.9 ms, targets
1920x1080 x72-90. Known risks: MSAA targets are skipped (left-quarter bug if
the title mixes MSAA and non-MSAA targets); copies between a halved and a
non-halved texture fail E_INVALIDARG silently; integer-coordinate UAV loads
in post effects may read wrong places. Rollback: unset VICTORY_TARGET_SCALE.

Demand-back pages: 1 MiB by default now (`VICTORY_DEMAND_PAGE_KB`), was 64 KiB —
the cost is per fault (~150 µs), not per byte; run 20's cutscene copied 39-48 MB
in ~600 faults for 93-96 ms. Watch for guard conflicts with the write watcher
(graceful: the page is skipped).
