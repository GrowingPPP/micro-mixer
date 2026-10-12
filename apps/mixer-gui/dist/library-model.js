/* Shared, pure mapping rules for the editable library and the loaded engine. */
(function (root) {
  // A missing key on an existing item means unbound. Keep the pair together
  // during edits so empty keys and custom bindings follow the sound's identity.
  const pairs = (items, keys) => items.map((item, index) => ({ item, key: keys[index] ?? '' }));
  const unpack = entries => ({ items: entries.map(e => e.item), keys: entries.map(e => e.key) });
  const reservedKeys = entries => new Set(entries.map(e => e.key.toUpperCase()));
  const defaultKey = (index, reserved) => {
    // F1..F24 are the function keys accepted by the native hotkey parser.
    // Start at the new position, then use another free key if an old binding
    // already uses it. A full set leaves the new sound unbound.
    for (let offset = 0; offset < 24; offset++) {
      const key = 'F' + ((index + offset) % 24 + 1);
      if (reserved.has(key)) continue;
      reserved.add(key);
      return key;
    }
    return '';
  };
  const model = {
    sourceIndex(slot) { return slot.source_index ?? slot.index; },
    baseSlots(state, keys) {
      if (state.running) return state.slots;
      return state.items.map((item, index) => ({
        index, source_index: index, id: item.id, name: item.name, file: item.file,
        duration: 0, hotkey: keys[index] || '', gain_db: item.gain,
      }));
    },
    itemForSlot(state, slot) { return state.items[model.sourceIndex(slot)]; },
    canPlay(state) { return !!state.running; },
    currentPlayback(state) {
      const status = state.lastStatus;
      if (!status) return null;
      if (status.preview_playing && status.preview_sound_id) {
        return { kind: 'preview', soundId: status.preview_sound_id,
          playhead: status.preview_playhead_secs, duration: status.preview_duration_secs };
      }
      if (!status.playing || status.active_slot < 0) return null;
      const slot = state.slots.find(slot => slot.index === status.active_slot);
      return { kind: 'main', soundId: slot?.id, slot: status.active_slot,
        playhead: status.playhead_secs, duration: slot?.duration };
    },
    playbackForSlot(state, slot) {
      const current = model.currentPlayback(state);
      const active = !!current && (current.soundId && slot.id
        ? current.soundId === slot.id
        : current.kind === 'main' && current.slot === slot.index);
      const progress = active && Number.isFinite(current.duration) && current.duration > 0
        && Number.isFinite(current.playhead)
        ? Math.max(0, Math.min(100, current.playhead / current.duration * 100)) : 0;
      return { active, kind: active ? current.kind : null, progress };
    },
    hotkeys(text) { return String(text).trim() ? String(text).split(',').map(s => s.trim()) : []; },
    tags(value) {
      // Structured tags may contain literal separators. Only legacy text is split.
      return Array.isArray(value) ? value.slice()
        : String(value ?? '').split(/[,|;，、；]+/).map(tag => tag.trim()).filter(Boolean);
    },
    tagsAfterEdit(original, initialText, editedText) {
      return editedText === initialText ? model.tags(original)
        : [...new Set(model.tags(editedText))];
    },
    gainSender(send) {
      const pending = new Map();
      return (soundId, db) => {
        const existing = pending.get(soundId);
        if (existing) { existing.latest = db; return existing.done; }
        const entry = { latest: db, done: null };
        pending.set(soundId, entry);
        entry.done = (async () => {
          while (entry.latest !== null) {
            const next = entry.latest;
            entry.latest = null;
            await send(soundId, next);
          }
        })().finally(() => pending.delete(soundId));
        return entry.done;
      };
    },
    remove(items, keys, index) {
      return unpack(pairs(items, keys).filter((_, i) => i !== index));
    },
    move(items, keys, index, to) {
      const entries = pairs(items, keys);
      if (index >= 0 && index < entries.length && to >= 0 && to < entries.length) {
        const [entry] = entries.splice(index, 1);
        entries.splice(to, 0, entry);
      }
      return unpack(entries);
    },
    sort(items, keys, compare) {
      return unpack(pairs(items, keys).sort((a, b) => compare(a.item, b.item)));
    },
    appendFiles(items, keys, files, createItem) {
      const entries = pairs(items, keys);
      const known = new Set(items.map(item => item.file));
      const reserved = reservedKeys(entries);
      for (const file of files) {
        if (known.has(file)) continue;
        known.add(file);
        entries.push({ item: createItem(file), key: defaultKey(entries.length, reserved) });
      }
      return unpack(entries);
    },
    scanFiles(items, keys, files, createItem) {
      const known = new Map(pairs(items, keys).map(entry => [entry.item.file, entry]));
      const reserved = reservedKeys(files.flatMap(file => known.has(file) ? [known.get(file)] : []));
      const seen = new Set();
      const entries = [];
      for (const file of files) {
        if (seen.has(file)) continue;
        seen.add(file);
        entries.push(known.get(file) ?? { item: createItem(file), key: defaultKey(entries.length, reserved) });
      }
      return unpack(entries);
    },
    resolveSelector(devices, selector, direction) {
      if (!selector) return '';
      const candidates = devices.filter(d => d.direction === direction);
      if (candidates.some(d => d.id === selector)) return selector;
      const matches = candidates.filter(d => d.name === selector);
      if (matches.length > 1) throw new Error(`设备名「${selector}」对应多个设备，请重新选择`);
      return matches.length === 1 ? matches[0].id : selector;
    },
    basename(path) { return String(path).split(/[\\/]/).pop().replace(/\.[^.]+$/, ''); },
  };
  if (typeof module !== 'undefined' && module.exports) module.exports = model;
  else root.MixerLibrary = model;
})(globalThis);
