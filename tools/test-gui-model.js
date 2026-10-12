const test = require('node:test');
const assert = require('node:assert/strict');
const model = require('../apps/mixer-gui/dist/library-model.js');

test('standby renders edits from the library instead of stale engine metadata', () => {
  const state = { running: false, slots: [{ index: 0, name: 'old', file: 'old.wav' }],
    items: [{ id: 'new-id', name: 'new', file: 'new.wav', gain: -12 }] };
  const slots = model.baseSlots(state, ['F3']);
  assert.equal(slots[0].file, 'new.wav');
  assert.equal(slots[0].id, 'new-id');
  assert.equal(slots[0].hotkey, 'F3');
  assert.equal(model.canPlay(state), false);
});

test('skipped files preserve the source identity for tags, gain and editing', () => {
  const state = { running: true, items: [{ id: 'bad' }, { id: 'good', gain: -18, tags: ['B-only'] }],
    slots: [{ index: 0, source_index: 1, id: 'good', file: 'good.wav' }] };
  assert.equal(model.itemForSlot(state, state.slots[0]).id, 'good');
  assert.deepEqual(model.itemForSlot(state, state.slots[0]).tags, ['B-only']);
  assert.equal(model.sourceIndex(state.slots[0]), 1);
  assert.equal(model.baseSlots(state, [])[0].index, 0);
  assert.equal(model.canPlay(state), true);
});

test('main playback selects the loaded slot identity after bad files were skipped', () => {
  const state = { running: true, items: [{ id: 'bad' }, { id: 'A' }, { id: 'B' }],
    slots: [{ index: 0, source_index: 1, id: 'A', duration: 4 },
      { index: 1, source_index: 2, id: 'B', duration: 2 }],
    lastStatus: { playing: true, active_slot: 1, playhead_secs: 0.5 } };
  assert.deepEqual(model.playbackForSlot(state, state.slots[0]), { active: false, kind: null, progress: 0 });
  assert.deepEqual(model.playbackForSlot(state, state.slots[1]), { active: true, kind: 'main', progress: 25 });
  // A standby library card can have a different index while retaining the stable sound identity.
  assert.equal(model.playbackForSlot(state, { index: 2, id: 'B' }).active, true);
});

test('preview identity and actual duration work without a running engine and survive library reorder', () => {
  const state = { running: false, slots: [], items: [{ id: 'B' }, { id: 'A' }],
    lastStatus: { playing: false, active_slot: -1,
      preview_sound_id: 'A', preview_playing: true, preview_playhead_secs: 3, preview_duration_secs: 4 } };
  const slots = model.baseSlots(state, []);
  assert.deepEqual(slots.map(slot => model.playbackForSlot(state, slot)), [
    { active: false, kind: null, progress: 0 }, { active: true, kind: 'preview', progress: 75 },
  ]);
  state.lastStatus.preview_playing = false;
  assert.equal(model.currentPlayback(state), null, 'a reserved ID during decode is not playing');
  assert.equal(model.playbackForSlot(state, slots[1]).active, false);
});

test('only one current playback is displayed and invalid progress never produces an invalid bar', () => {
  const state = { slots: [{ index: 0, id: 'A', duration: 2 }, { index: 1, id: 'B', duration: 2 }],
    lastStatus: { playing: true, active_slot: 0, playhead_secs: 1,
      preview_sound_id: 'B', preview_playing: true, preview_playhead_secs: 3, preview_duration_secs: 2 } };
  assert.deepEqual(state.slots.map(slot => model.playbackForSlot(state, slot).active), [false, true]);
  assert.equal(model.playbackForSlot(state, state.slots[1]).progress, 100);
  state.lastStatus.preview_playhead_secs = -1;
  assert.equal(model.playbackForSlot(state, state.slots[1]).progress, 0);
  state.lastStatus.preview_duration_secs = NaN;
  assert.equal(model.playbackForSlot(state, state.slots[1]).progress, 0);
  state.lastStatus.preview_playing = false;
  state.lastStatus.playing = false;
  assert.equal(model.currentPlayback(state), null);
});

test('Windows and macOS paths use the same filename rule', () => {
  assert.equal(model.basename('C:\\Users\\Doe, John\\嘿,你好吗.wav'), '嘿,你好吗');
  assert.equal(model.basename('/Users/ian/Sounds/嘿,你好吗.wav'), '嘿,你好吗');
});

test('empty hotkeys preserve source slot positions', () => {
  assert.deepEqual(model.hotkeys(',F2, ,F4'), ['', 'F2', '', 'F4']);
  assert.deepEqual(model.hotkeys('   '), []);
});

test('legacy device names migrate only when unambiguous and keep direction', () => {
  const devices = [{ id: 'mic-1', name: 'Shared name', direction: '输入' },
    { id: 'out-1', name: 'Shared name', direction: '输出' }];
  assert.equal(model.resolveSelector(devices, 'Shared name', '输入'), 'mic-1');
  assert.equal(model.resolveSelector(devices, 'out-1', '输出'), 'out-1');
  devices.push({ id: 'mic-2', name: 'Shared name', direction: '输入' });
  assert.throws(() => model.resolveSelector(devices, 'Shared name', '输入'), /多个设备/);
});

const sound = (id, name = id) => ({ id, file: id + '.wav', name, gain: -12, tags: [id + '-tag'] });
const createSound = file => sound(file.slice(0, -4));

test('removing A preserves B and its F2 binding without mutating the source library', () => {
  const items = [sound('A'), sound('B')];
  const keys = ['F1', 'F2'];
  const result = model.remove(items, keys, 0);
  assert.deepEqual(result, { items: [items[1]], keys: ['F2'] });
  assert.equal(result.items[0].id, 'B');
  assert.equal(items.length, 2);
  assert.deepEqual(keys, ['F1', 'F2']);
});

test('moving sounds preserves custom keys, empty slots and absent unbound keys', () => {
  const items = [sound('A'), sound('B'), sound('C')];
  const result = model.move(items, ['Ctrl+K', '', 'Alt+2'], 0, 2);
  assert.deepEqual(result.items.map(item => item.id), ['B', 'C', 'A']);
  assert.deepEqual(result.keys, ['', 'Alt+2', 'Ctrl+K']);
  const unbound = model.move(items, [], 0, 2);
  assert.deepEqual(unbound.keys, ['', '', '']);
  const partial = model.move(items, ['F1'], 2, 0);
  assert.deepEqual(partial.keys, ['', 'F1', '']);
  assert.deepEqual(items.map(item => item.id), ['A', 'B', 'C']);
});

test('sorting carries custom and empty hotkeys with sound identity and metadata', () => {
  const items = [sound('C'), sound('A'), sound('B')];
  const result = model.sort(items, ['Ctrl+K', '', 'Alt+2'], (a, b) => a.name.localeCompare(b.name));
  assert.deepEqual(result.items, [items[1], items[2], items[0]]);
  assert.deepEqual(result.keys, ['', 'Alt+2', 'Ctrl+K']);
  assert.deepEqual(result.items[0].tags, ['A-tag']);
  assert.equal(result.items[2].gain, -12);
  assert.deepEqual(items.map(item => item.id), ['C', 'A', 'B']);
});

test('appending gives only new files default keys and keeps old bindings including empty ones', () => {
  const items = [sound('A'), sound('B')];
  const created = [];
  const result = model.appendFiles(items, ['Ctrl+K', ''], ['B.wav', 'C.wav', 'C.wav', 'D.wav'], file => {
    created.push(file);
    return createSound(file);
  });
  assert.deepEqual(result.items.map(item => item.id), ['A', 'B', 'C', 'D']);
  assert.deepEqual(result.keys, ['Ctrl+K', '', 'F3', 'F4']);
  assert.deepEqual(created, ['C.wav', 'D.wav']);
  assert.equal(result.items[0], items[0]);
  assert.equal(result.items[1], items[1]);
  assert.deepEqual(model.appendFiles(items, [], ['C.wav'], createSound).keys, ['', '', 'F3']);
});

test('scanning reuses known file identity, metadata and keys through reordering', () => {
  const items = [sound('A'), sound('B'), sound('removed')];
  const created = [];
  const result = model.scanFiles(items, ['Ctrl+K', '', 'Alt+2'], ['B.wav', 'C.wav', 'A.wav', 'C.wav'], file => {
    created.push(file);
    return createSound(file);
  });
  assert.deepEqual(result.items.map(item => item.id), ['B', 'C', 'A']);
  assert.deepEqual(result.keys, ['', 'F2', 'Ctrl+K']);
  assert.equal(result.items[0], items[1]);
  assert.equal(result.items[2], items[0]);
  assert.deepEqual(created, ['C.wav']);
  assert.deepEqual(model.scanFiles(items, [], ['B.wav', 'A.wav'], createSound).keys, ['', '']);
  assert.deepEqual(model.scanFiles(items, ['F1', 'F2'], [], createSound), { items: [], keys: [] });
});

test('new default keys avoid retained bindings and stop at the supported F24', () => {
  const items = [sound('A')];
  assert.deepEqual(model.appendFiles(items, ['f2'], ['B.wav'], createSound).keys, ['f2', 'F3']);
  const scan = model.scanFiles(items, ['F2'], ['A.wav', 'B.wav'], createSound);
  assert.deepEqual(scan.keys, ['F2', 'F3']);
  const full = Array.from({ length: 24 }, (_, i) => sound('old-' + i));
  const fullKeys = full.map((_, i) => 'F' + (i + 1));
  const appended = model.appendFiles(full, fullKeys, ['new.wav'], createSound);
  assert.deepEqual(appended.keys, [...fullKeys, '']);
});

test('structured tag arrays preserve literal punctuation and are cloned', () => {
  const tags = ['a,b', 'semi;colon', 'x|y', '中文，字面；文本、分隔', '标签 "引号"', ''];
  const result = model.tags(tags);
  assert.deepEqual(result, tags);
  assert.notEqual(result, tags);
  result.push('new');
  assert.equal(tags.length, 6);
  assert.deepEqual(model.tags([]), []);
});

test('legacy tag strings adapt all supported separators into an array', () => {
  assert.deepEqual(model.tags(' A, B; C| D， 中文；标签、声音 '), ['A', 'B', 'C', 'D', '中文', '标签', '声音']);
  assert.deepEqual(model.tags(''), []);
  assert.deepEqual(model.tags(null), []);
});

test('literal tags survive load, move, scan and save DTO serialization', () => {
  const originalTags = ['a,b', 'semi;colon', 'x|y', '中文，字面；文本、分隔'];
  const dto = [{ ...sound('A'), tags: originalTags }, { ...sound('B'), tags: [] }];
  const loaded = JSON.parse(JSON.stringify(dto)).map(item => ({ ...item, tags: model.tags(item.tags) }));
  const moved = model.move(loaded, ['', 'F2'], 0, 1);
  assert.deepEqual(moved.items[1].tags, originalTags);
  const scanned = model.scanFiles(moved.items, moved.keys, ['B.wav', 'C.wav', 'A.wav'], createSound);
  const saved = JSON.parse(JSON.stringify(scanned.items.map(item => ({ ...item, tags: item.tags }))));
  assert.deepEqual(saved[2], dto[0]);
  assert.deepEqual(saved[0].tags, []);
  assert.deepEqual(scanned.keys, ['F2', 'F3', '']);
  assert.ok(saved.every(item => Array.isArray(item.tags)));
});

test('unchanged tag editing preserves single comma tags and other literal separators', () => {
  const tags = ['a,b', 'semi;colon', 'x|y', '中文，字面；文本、分隔'];
  const initialText = tags.join(',');
  assert.deepEqual(model.tagsAfterEdit(tags, initialText, initialText), tags);
  assert.deepEqual(model.tagsAfterEdit(['a,b'], 'a,b', 'a,b'), ['a,b']);
  assert.deepEqual(model.tagsAfterEdit([], '', ''), []);
});

test('changed tag editing parses separators and deduplicates user input', () => {
  const tags = ['a,b'];
  assert.deepEqual(model.tagsAfterEdit(tags, 'a,b', '新标签,声音;游戏|笑声，游戏；中文、声音'),
    ['新标签', '声音', '游戏', '笑声', '中文']);
  assert.deepEqual(model.tagsAfterEdit(tags, 'a,b', ''), []);
  assert.deepEqual(tags, ['a,b']);
});
