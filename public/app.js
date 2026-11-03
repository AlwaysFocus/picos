// Guest inspection uses GET. Explicit operator controls can grant turns or change session policy.
// Execution and resource enforcement remain in Rust.
const $ = (id) => document.getElementById(id);
const canvas = $('display');
const context = canvas.getContext('2d', { alpha: false });
const pixels = context.createImageData(256, 192);
const pageBytes = 256;
const previousBytes = new Map();
const changedUntil = new Map();
let state = null;
let viewAddress = 0;
let followPc = false;
let followWrites = true;
let memoryEvents = [];
let lastChangeId = 0;
let selectedChange = null;
let hotRanges = [];
let focusAddress = null;
let focusVersion = 0;
let lastFocusVersion = -1;
let historySignature = '';
let mapSignature = '';
const highlightMs = 3600;
let showBytes = true;
let requestInFlight = false;
let refreshAgain = false;
let refreshTimer;
let navigationVersion = 0;
let lastExperiment;
let lastPc;
let lastRenderedAddress;
let lastSerial = '';
let memoryError = '';
let controlsBusy = false;
let modeDirty = false;
let controlConnection = false;

// The LAN key is a browser capability, never part of an agent prompt or URL request.
let accessKey = '';
let accessRequired = false;
try { accessKey = sessionStorage.getItem('picos-access') || ''; } catch {}
const linkKey = new URLSearchParams(location.hash.slice(1)).get('access');
if (linkKey) {
  accessKey = linkKey;
  try { sessionStorage.setItem('picos-access', accessKey); } catch {}
  history.replaceState(null, '', location.pathname + location.search);
}
function accessHeaders() { return accessKey ? { 'X-Picos-Access': accessKey } : {}; }
function requireAccess() {
  accessRequired = true;
  document.body.dataset.accessRequired = 'true';
  $('access-panel').hidden = false;
  $('access-feedback').textContent = accessKey ? 'This key has expired or is incorrect. Use the current link from your terminal.' : '';
  connection(false);
  error('');
}
const phoneLayout = matchMedia('(max-width: 900px)');
let mobileView = 'display';
function setMobileView(view, updateInspector = true) {
  mobileView = view;
  document.body.dataset.mobileView = view;
  for (const button of document.querySelectorAll('.mobile-nav button')) button.setAttribute('aria-pressed', String(button.dataset.view === view));
  if (updateInspector && ['memory', 'journal'].includes(view)) setInspector(view, false);
  if (phoneLayout.matches) {
    journalActive = view === 'journal';
    window.scrollTo({ top: 0, behavior: 'instant' });
  }
  if (journalActive && journalFollowing) requestAnimationFrame(scrollJournalBottom);
}
phoneLayout.addEventListener('change', () => {
  journalActive = phoneLayout.matches ? mobileView === 'journal' : !$('journal-view').hidden;
});

function hex(value) { return Number(value || 0).toString(16).padStart(8, '0'); }
function count(value) { return Number(value || 0).toLocaleString(); }
function compact(value) { return new Intl.NumberFormat('en', { notation: 'compact', maximumFractionDigits: 1 }).format(value || 0); }
function mb(value) { return new Intl.NumberFormat('en', { maximumFractionDigits: 3 }).format(Number(value || 0) / 1e6); }
function byteCount(value) { return value >= 1000 ? `${compact(value)} B` : `${count(value)} B`; }
function error(message) {
  const displayed = String(message).slice(0, 1600);
  if ($('error').textContent !== displayed) $('error').textContent = displayed;
  $('error').hidden = !displayed;
}
function ramBytes() { return Number(state?.machine?.config?.ramBytes || state?.config?.ramBytes || 0); }
function clampAddress(address) { return Math.max(0, Math.min(address, Math.max(0, ramBytes() - 8))); }
async function request(path) {
  const response = await fetch(path, { headers: accessHeaders(), cache: 'no-store', signal: AbortSignal.timeout(10000) });
  if (!response.ok) {
    const result = await response.json().catch(() => ({}));
    if (response.status === 401 && result.code === 'access_required') requireAccess();
    throw new Error(result.error || `Observer request failed (${response.status})`);
  }
  return response;
}
function connection(connected) {
  controlConnection = connected;
  if (state) renderSessionControls(state);
  $('connection').dataset.state = connected ? 'live' : 'offline';
  const label = connected ? 'Live' : 'Offline';
  if ($('connection-label').textContent !== label) $('connection-label').textContent = label;
}
function setFollow(mode) {
  followPc = mode === 'pc';
  followWrites = mode === 'writes';
  $('follow-pc').setAttribute('aria-pressed', String(followPc));
  $('follow-writes').setAttribute('aria-pressed', String(followWrites));
  $('follow-mode').textContent = followWrites ? 'Following changes' : followPc ? 'Following execution' : 'Manual inspection';
}
function age(time) {
  const seconds = Math.max(0, Math.floor((Date.now() - Number(time)) / 1000));
  if (seconds < 2) return 'just now';
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  return `${Math.floor(seconds / 3600)}h ago`;
}
function changeRange(event) {
  // Prefer instructions/data over pixels when an operation changes both. RAM does
  // not carry code types, so this is only a framebuffer distinction.
  const ranges = event?.ranges || [];
  const videoBase = ramBytes() - 256 * 192;
  const candidates = event?.kind === 'assemble' ? ranges : ranges.slice().reverse();
  return candidates.find((range) => Number(range.address) < videoBase) || ranges[0];
}
function selectChange(event, follow = false) {
  selectedChange = event;
  const range = changeRange(event);
  if (range) {
    focusAddress = Number(range.address);
    if (follow) viewAddress = clampAddress(Math.floor(focusAddress / 8) * 8);
    focusVersion++;
  }
}
function bytesText(bytes) { return (bytes || []).slice(0, 12).map((v) => Number(v).toString(16).padStart(2, '0')).join(' '); }
function renderChangeFocus() {
  const event = selectedChange;
  if (!event) return;
  const range = changeRange(event);
  const names = { assemble: 'ASSEMBLY LOADED', write: 'RAM PATCHED', run: 'GUEST WROTE RAM', boot: 'MACHINE BOOTED', step: 'GUEST WROTE RAM' };
  $('change-kind').textContent = names[event.kind] || 'MEMORY CHANGED';
  $('change-time').textContent = age(event.time);
  $('change-label').textContent = event.label || `${byteCount(event.changedBytes)} changed in RAM`;
  $('change-label').title = event.label || '';
  $('change-address').textContent = range ? `@ ${hex(range.address)} · ${event.ranges.length} range${event.ranges.length === 1 ? '' : 's'}${event.truncated ? '+' : ''}` : 'No preview available';
  $('change-size').textContent = `${byteCount(event.changedBytes)} changed`;
  $('byte-diff').hidden = !range?.before?.length;
  $('byte-diff').title = 'Recorded byte preview for this operation. Disassembly below always shows current RAM.';
  $('bytes-before').textContent = bytesText(range?.before);
  $('bytes-after').textContent = bytesText(range?.after);
  $('change-focus').dataset.fresh = String(Date.now() - Number(event.time) < highlightMs);
}
function renderMemoryActivity(next) {
  const activity = next.memoryActivity || {};
  memoryEvents = Array.isArray(activity.events) ? activity.events : [];
  const unseen = memoryEvents.filter((event) => Number(event.id) > lastChangeId);
  const now = Date.now();
  for (const event of unseen) {
    const expires = Number(event.time) + highlightMs;
    if (expires > now) {
      for (const range of event.ranges || []) hotRanges.push({ start: Number(range.address), end: Number(range.address) + Number(range.length), expires });
    }
  }
  hotRanges = hotRanges.filter((range) => range.expires > now).slice(-512);
  if (unseen.length) {
    lastChangeId = Math.max(lastChangeId, ...unseen.map((event) => Number(event.id)));
    if (followWrites || !selectedChange) selectChange(unseen.at(-1), followWrites);
  }
  $('write-count').textContent = `${count(activity.revision || lastChangeId)} memory updates`;
  const latest = memoryEvents.at(-1);
  $('last-write').textContent = latest ? `Last write ${age(latest.time)}` : 'No writes yet';
  $('map-end').textContent = `${mb(ramBytes())} MB`;
  renderChangeFocus();
  const signature = `${memoryEvents.map((event) => event.id).join(',')}:${selectedChange?.id}`;
  if (signature !== historySignature) {
    historySignature = signature;
    const history = document.createDocumentFragment();
    for (const event of memoryEvents.slice(-8).reverse()) {
      const range = changeRange(event);
      const button = document.createElement('button');
      button.className = `change-event${event.id === selectedChange?.id ? ' selected' : ''}`;
      button.disabled = !range;
      button.setAttribute('aria-label', `${event.kind}, ${byteCount(event.changedBytes)} changed. Inspect current RAM at ${hex(range?.address)}`);
      const title = document.createElement('strong'); title.textContent = event.kind === 'assemble' ? 'Assembly loaded' : event.kind === 'run' ? 'Guest writes' : 'Memory changed';
      const location = document.createElement('span'); location.textContent = `@ ${hex(range?.address)}`;
      const size = document.createElement('span'); size.textContent = `${byteCount(event.changedBytes)} · #${event.id}`;
      button.append(title, location, size);
      button.addEventListener('click', () => { setInspector('memory'); setFollow('manual'); selectChange(event); navigate(Math.floor(Number(range.address) / 8) * 8); renderMemoryActivity(state); });
      history.append(button);
    }
    if (memoryEvents.length) $('change-history').replaceChildren(history);
  }
  // Bucket recorded ranges so the map remains legible and keyboard navigation
  // stays bounded. Keep the same nodes between writes instead of stealing focus.
  const mapKey = memoryEvents.slice(-12).map((event) => event.id).join(',');
  if (mapKey !== mapSignature) {
    mapSignature = mapKey;
    const bins = new Map();
    const binSize = Math.max(1, ramBytes() / 64);
    for (const event of memoryEvents.slice(-12)) {
      for (const range of event.ranges || []) {
        const first = Math.max(0, Math.floor(Number(range.address) / binSize));
        const last = Math.min(63, Math.floor((Number(range.address) + Number(range.length) - 1) / binSize));
        for (let bin = first; bin <= last; bin++) bins.set(bin, { event, range });
      }
    }
    const heat = document.createDocumentFragment();
    for (const [bin, { event, range }] of bins) {
      const mark = document.createElement('button');
      const address = Math.max(Number(range.address), Math.floor(bin * binSize / 8) * 8);
      mark.className = 'heat-mark';
      mark.dataset.time = String(event.time);
      mark.style.left = `${bin / 64 * 100}%`;
      mark.style.width = `${100 / 64}%`;
      mark.title = `Recent ${event.kind} in this RAM region. Inspect 0x${hex(address)}.`;
      mark.setAttribute('aria-label', mark.title);
      mark.addEventListener('click', () => { selectChange(event); navigate(address); });
      heat.append(mark);
    }
    $('memory-heat').replaceChildren(heat);
  }
  for (const mark of $('memory-heat').children) mark.dataset.fresh = String(now - Number(mark.dataset.time) < highlightMs);
}
// Public agent output and operator-visible prompts are a separate observation
// stream. Never derive or render private reasoning from transport payloads.
let journalEntries = [];
let journalCursor = 0;
let journalExperiment;
let journalMission;
let journalFollowing = true;
let journalActive = true;
let journalUnread = 0;
let journalBusy = false;
let journalTimer;
let journalGeneration = 0;
let journalPromptKey = '';
let journalFilter = 'all';
const journalTypes = new Set(['agent.message', 'agent.turn', 'agent.start', 'agent.completed', 'agent.stopped', 'agent.failed', 'agent.error', 'agent.limit', 'agent.session', 'agent.warning', 'agent.tool', 'tool', 'tool-error', 'assemble', 'operator.settings', 'operator.turns', 'operator.continue', 'operator.stop', 'system']);
function setInspector(view, updateMobile = true) {
  journalActive = view === 'journal';
  if (updateMobile && phoneLayout.matches) setMobileView(view, false);
  $('memory-view').hidden = journalActive;
  $('journal-view').hidden = !journalActive;
  for (const [id, active] of [['memory-tab', !journalActive], ['journal-tab', journalActive]]) {
    $(id).setAttribute('aria-selected', String(active)); $(id).tabIndex = active ? 0 : -1;
  }
  if (journalActive) { journalUnread = 0; updateJournalBadge(); if (journalFollowing) scrollJournalBottom(); }
}
function updateJournalBadge() {
  $('journal-count').textContent = count(journalEntries.length);
  $('journal-unread').hidden = !journalUnread;
  $('journal-unread').textContent = `+${count(journalUnread)}`;
  $('mobile-journal-unread').hidden = !journalUnread;
  $('mobile-journal-unread').textContent = count(Math.min(journalUnread, 99));
}
function updateGoal(mission) {
  if (typeof mission !== 'string') return;
  if ($('goal-text').textContent !== mission) $('goal-text').textContent = mission || 'No goal was supplied.';
  $('copy-goal').disabled = !mission;
}
function setJournalFollow(value) {
  journalFollowing = value;
  $('journal-follow').setAttribute('aria-pressed', String(value));
  $('journal-follow').textContent = value ? 'Following latest' : 'Follow latest';
}
function scrollJournalBottom() { const scroller = $('journal-scroll'); scroller.scrollTop = scroller.scrollHeight; }
function visibleJournalEntry(entry) {
  if (!journalTypes.has(entry.type)) return false;
  if (journalFilter === 'messages') return entry.type === 'agent.message';
  if (journalFilter === 'tools') return ['agent.tool', 'tool', 'tool-error', 'assemble'].includes(entry.type);
  return true;
}
function journalTime(value) {
  const date = new Date(Number(value));
  return Number.isFinite(date.getTime()) ? date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }) : 'Time unavailable';
}
function journalLabel(type) {
  return ({ 'agent.message': 'Agent message', 'agent.turn': 'Turn started', 'agent.start': 'Connected', 'agent.completed': 'Turn completed', 'agent.stopped': 'Stopped', 'agent.failed': 'Failed', 'agent.error': 'Agent error', 'agent.limit': 'Limit reached', 'agent.session': 'Session update', 'agent.warning': 'Agent notice', 'agent.tool': 'Tool activity', tool: 'Tool result', 'tool-error': 'Tool error', assemble: 'Assembly loaded', system: 'System' })[type] || 'Operator action';
}
function createJournalEntry(entry) {
  const item = document.createElement('li'); item.className = `journal-entry${entry.type === 'agent.message' ? ' journal-message' : ''}`; item.dataset.id = String(entry.id);
  const heading = document.createElement('div'); heading.className = 'journal-entry-heading';
  const type = document.createElement('strong'); type.textContent = journalLabel(entry.type);
  const turn = document.createElement('span'); turn.textContent = entry.turn == null ? (entry.imported ? 'Earlier record' : 'Session') : `Turn ${count(entry.turn)}`;
  if (entry.sessionId) turn.title = `Session ${entry.sessionId}`;
  const time = document.createElement('time'); time.textContent = journalTime(entry.time); time.title = new Date(Number(entry.time)).toString();
  heading.append(type, turn, time);
  const message = document.createElement('p'); message.className = 'journal-message-text'; message.textContent = typeof entry.message === 'string' ? entry.message : '';
  item.append(heading, message);
  if (entry.truncated) { const note=document.createElement('p'); note.className='journal-retention'; note.textContent='Long message truncated by the recorder.'; item.append(note); }
  if (entry.imported) item.title='Imported from earlier public events. Older messages may have been shortened by the previous recorder.';
  if (entry.details && ['tool', 'tool-error'].includes(entry.type)) {
    const details = document.createElement('details'); details.className = 'tool-evidence';
    const summary = document.createElement('summary'); summary.textContent = 'Arguments and result previews';
    const content = document.createElement('pre'); content.textContent = JSON.stringify(entry.details, null, 2);
    details.append(summary, content); item.append(details);
  }
  return item;
}
function renderJournalEntries(rebuild = false, additions = []) {
  const list = $('journal-entries'); const scroller = $('journal-scroll');
  const anchor = [...list.children].find((node) => node.offsetTop + node.offsetHeight > scroller.scrollTop);
  const anchorId = anchor?.dataset.id; const anchorOffset = anchor ? anchor.offsetTop - scroller.scrollTop : 0;
  if (rebuild) list.replaceChildren(...journalEntries.filter(visibleJournalEntry).map(createJournalEntry));
  else {
    const minimum = journalEntries[0]?.id || 0;
    for (const node of [...list.children]) if (Number(node.dataset.id) < minimum) node.remove();
    for (const entry of additions) if (visibleJournalEntry(entry)) list.append(createJournalEntry(entry));
  }
  $('journal-empty').hidden = list.childElementCount > 0;
  $('journal-empty').textContent = journalEntries.length ? 'No entries match this filter.' : 'No public activity recorded yet. The model may be working without emitting a message.';
  if (journalFollowing && journalActive) scrollJournalBottom();
  else if (anchorId) {
    const retained = [...list.children].find((node) => node.dataset.id === anchorId);
    if (retained) scroller.scrollTop = retained.offsetTop - anchorOffset;
  }
  updateJournalBadge();
}
function renderSubmittedPrompts(turns) {
  const bounded = Array.isArray(turns) ? turns.slice(-24) : [];
  const key = bounded.map((turn) => `${turn.id}:${turn.sessionId}:${turn.turn}:${turn.time}`).join('|') || 'empty';
  if (key === journalPromptKey) return;
  journalPromptKey = key;
  const container = $('submitted-prompts');
  const existing = new Map([...container.children].map((node) => [node.dataset.key, node]));
  const fragment = document.createDocumentFragment();
  for (const turn of bounded.slice().reverse()) {
    const identity = `${turn.id}:${turn.sessionId}:${turn.turn}:${turn.time}`;
    let detail = existing.get(identity);
    if (!detail) {
      detail = document.createElement('details'); detail.className = 'submitted-prompt'; detail.dataset.key = identity;
      const summary = document.createElement('summary'); summary.textContent = `Turn ${count(turn.turn)} · ${journalTime(turn.time)} · ${turn.model || 'Codex default'}`;
      if (turn.sessionId) summary.title = `Session ${turn.sessionId}. Picos-authored instructions sent through ${turn.backend === 'chatgpt-direct' ? 'direct ChatGPT' : 'Codex'}; not the model's complete system context.`;
      const prompt = document.createElement('pre'); prompt.textContent = typeof turn.prompt === 'string' ? turn.prompt + (turn.opening ? `\n\nOPENING USER MESSAGE:\n${turn.opening}` : '') : 'No prompt recorded.';
      detail.append(summary, prompt);
    }
    fragment.append(detail);
  }
  if (!bounded.length) { const empty = document.createElement('p'); empty.className = 'journal-empty'; empty.textContent = 'Prompt snapshots begin with new turns. Earlier prompts were not recorded.'; fragment.append(empty); }
  container.replaceChildren(fragment);
}
function resetJournal(experiment) {
  journalGeneration++; journalExperiment = experiment; journalMission = undefined; journalCursor = 0; journalEntries = []; journalUnread = 0; journalPromptKey = '';
  $('journal-entries').replaceChildren(); $('submitted-prompts').replaceChildren(); updateJournalBadge();
  $('journal-empty').hidden = false; $('journal-empty').textContent = 'Loading recorded agent activity…';
}
async function refreshJournal() {
  if (journalBusy || accessRequired) return;
  journalBusy = true;
  const generation = journalGeneration;
  try {
    let response = await request(`/api/agent/journal?after=${journalCursor}`).then((result) => result.json());
    if (generation !== journalGeneration) return;
    if (response.experiment && response.experiment !== journalExperiment) resetJournal(response.experiment);
    if (journalCursor && (Number(response.latestId) < journalCursor || Number(response.oldestId) > journalCursor + 1)) {
      journalCursor = 0; journalEntries = []; $('journal-entries').replaceChildren();
      response = await request('/api/agent/journal?after=0').then((result) => result.json());
    }
    if (state?.experiment && response.experiment && response.experiment !== state.experiment) return;
    journalMission = typeof response.mission === 'string' ? response.mission : journalMission;
    updateGoal(journalMission);
    $('initial-goal').hidden = !response.initialMission || response.initialMission === response.mission;
    $('initial-goal-text').textContent = typeof response.initialMission === 'string' ? response.initialMission : '';
    const incoming = (Array.isArray(response.entries) ? response.entries : []).filter((entry) => Number(entry.id) > journalCursor && journalTypes.has(entry.type));
    const seen = new Set(journalEntries.map((entry) => entry.id));
    const additions = incoming.filter((entry) => !seen.has(entry.id)).sort((a, b) => Number(a.id) - Number(b.id));
    journalEntries.push(...additions); journalEntries = journalEntries.slice(-400);
    journalCursor = Math.max(journalCursor, Number(response.latestId) || 0, ...incoming.map((entry) => Number(entry.id)));
    if (!journalActive) journalUnread += additions.length;
    renderJournalEntries(false, additions.filter((entry) => entry.id >= (journalEntries[0]?.id || 0)));
    renderSubmittedPrompts(response.turns);
    $('journal-retention').textContent = Number(response.oldestId) > 1 ? 'Showing recent activity. Older records remain in the experiment journal.' : 'Public messages, tool actions, and outcomes in chronological order.';
    $('journal-error').hidden = true;
  } catch (failure) {
    if (accessRequired) return;
    $('journal-error').textContent = `Journal unavailable: ${failure.message}`; $('journal-error').hidden = false;
  } finally {
    journalBusy = false; clearTimeout(journalTimer); if (!accessRequired) journalTimer = setTimeout(refreshJournal, document.hidden ? 5000 : 1500);
  }
}

const modeDescriptions = {
  standard: 'Build across a fixed number of turns. Grant more turns to keep going.',
  'resource-limit': 'Keep building until a hard resource boundary is reached, then stop immediately. No turn ceiling.',
  maximize: 'Use the allotted turns to build the best target functionality within the hardware limits. Improve quality, not filler.',
};
function controlFeedback(message, failed = false) {
  $('controls-feedback').textContent = message;
  $('controls-feedback').hidden = !message;
  $('controls-feedback').dataset.error = String(failed);
}
function renderSessionControls(next) {
  const session = next.agent?.session || {};
  const controls = next.sessionControls || {};
  const mode = session.mode || next.experimentDetails?.mode || 'standard';
  const available = Boolean(next.operatorToken) && controlConnection && !controlsBusy;
  if (!modeDirty && !controlsBusy) $('run-mode').value = mode;
  const selected = $('run-mode').value;
  $('run-mode').disabled = !available || !controls.canChangeMode;
  $('apply-mode').disabled = !available || !controls.canChangeMode || selected === mode;
  $('mode-description').textContent = modeDescriptions[selected] || modeDescriptions.standard;
  $('run-mode').title = controls.canChangeMode ? 'Choose a mode, then apply. Applying does not start the agent.' : 'Mode can be changed when the agent is stopped.';
  $('grant-turns').hidden = mode !== 'standard';
  $('continue-session').hidden = mode === 'standard' && !controls.canContinue;
  $('continue-session').textContent = session.status === 'failed' ? 'Resume after failure' : mode === 'standard' ? 'Resume remaining turns' : 'Continue experiment';
  const maximum = Math.min(24, Number(session.maxAdditionalTurns ?? 24));
  $('additional-turns').max = String(Math.max(1, maximum));
  $('additional-turns').disabled = !available || !session.canAddTurns || maximum < 1;
  $('add-turns').disabled = !available || !session.canAddTurns || maximum < 1;
  $('continue-session').disabled = !available || !controls.canContinue;
  const active = ['running', 'starting', 'stopping'].includes(next.agent?.status) || session.status === 'running';
  $('stop-session').hidden = !active;
  $('stop-session').disabled = !available || !active || next.agent?.status === 'stopping';
  $('turn-action-note').textContent = session.status === 'failed' && controls.canContinue
    ? 'Resume uses the remaining allowance. Saved work and consumed resources are preserved.'
    : session.status === 'failed' && session.canAddTurns
      ? 'The failed turn used the last allowance. Grant more turns to resume the saved machine.'
      : session.stopReason === 'resource_limit'
        ? 'More turns cannot override a hard resource stop.'
        : mode === 'standard'
          ? (session.canAddTurns ? (active ? 'Extends this run. Hardware limits stay fixed.' : 'Grants turns and resumes the saved machine.') : 'No additional turns are available with the current session allowance.')
          : (controls.canContinue ? 'Starts work in the selected mode on this machine.' : active ? 'The agent is already working.' : 'No continuation is available at this limit.');
  if (!controlsBusy) $('tell-turns').checked = session.tellTurns ?? next.experimentDetails?.tellTurns ?? true;
  $('tell-turns').disabled = !available || mode === 'resource-limit';
  $('turn-disclosure-note').textContent = mode === 'resource-limit'
    ? 'Resource-limit mode has no turn ceiling to disclose.'
    : 'Applies to the next turn. Previously disclosed budgets cannot be erased.';
}
async function sessionControl(path, body, successMessage) {
  if (controlsBusy || !state?.operatorToken || !controlConnection) return false;
  controlsBusy = true;
  controlFeedback('Applying…');
  renderSessionControls(state);
  try {
    const response = await fetch(path, {
      method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Picos-Operator': state.operatorToken, ...accessHeaders() },
      body: JSON.stringify(body), signal: AbortSignal.timeout(30000),
    });
    const result = await response.json().catch(() => ({}));
    if (response.status === 401 && result.code === 'access_required') requireAccess();
    if (!response.ok) throw new Error(result.error || `Experiment control failed (${response.status})`);
    modeDirty = false;
    // Read the authoritative result before enabling another operation. No retry
    // of a POST: a lost response must never silently grant turns twice.
    const updated = await request('/api/state').then((value) => value.json());
    renderState(updated);
    controlFeedback(successMessage);
    return true;
  } catch (failure) {
    const uncertain = failure.name === 'TimeoutError' || failure.name === 'AbortError' || failure instanceof TypeError;
    controlFeedback(uncertain ? 'Connection interrupted. Check the turn count before trying again; the request may have been applied.' : failure.message, true);
    void refresh();
    return false;
  } finally {
    controlsBusy = false;
    if (state) renderSessionControls(state);
  }
}
function renderAgent(next) {
  const agent = next.agent || {};
  const session = agent.session || {};
  const status = agent.status || 'idle';
  const events = Array.isArray(next.events) ? next.events : [];
  const latest = events.at(-1);
  const lastTool = events.findLast((event) => event.type === 'agent.tool');
  const writing = ['running', 'starting'].includes(status) && hotRanges.length > 0;
  const toolWorking = lastTool && /: (running|in_progress|started)$/.test(lastTool.message || '') && Number(lastTool.time) >= Number(events.findLast((event) => event.type === 'agent.start')?.time || 0);
  let phase = status;
  let title = { idle: 'Waiting for the agent', starting: 'Connecting to Codex', running: 'Thinking / planning the next change', stopping: 'Stopping the agent', stopped: 'Agent stopped', completed: 'Turn finished', failed: 'Agent failed' }[status] || status;
  let detail = agent.error || (status === 'running'
    ? `${lastTool ? `Last tool: ${lastTool.message}. ` : ''}No new RAM writes right now; the agent is preparing its next operation.`
    : session.reason || 'The observer remains live. Current machine state is preserved.');
  if (writing) { phase = 'writing'; title = 'Memory is changing'; detail = memoryEvents.at(-1)?.label || 'New bytes have reached guest RAM.'; }
  else if (status === 'running' && toolWorking) { phase = 'tool'; title = 'Using a machine tool'; detail = lastTool.message; }
  if (session.status && !['running', 'idle'].includes(session.status)) {
    phase = session.status;
    title = { continuing: 'Preparing the next turn', waiting: 'Preparing the next turn', budget: 'Experiment budget reached', completed: 'Experiment session finished', stalled: 'Paused: no machine progress', stopped: 'Experiment stopped', failed: 'Experiment needs attention' }[session.status] || title;
    detail = session.reason || detail;
    if (session.status === 'failed') {
      const failure = agent.error || events.findLast(event => event.type === 'agent.failed' || event.type === 'agent.error')?.message;
      const recovery = next.sessionControls?.canContinue ? 'Use Resume after failure in Controls to continue with the remaining turns.'
        : session.canAddTurns ? 'Grant more turns in Controls to continue.' : '';
      detail = [failure || detail, recovery].filter(Boolean).join(' ');
    }
  }
  $('agent-panel').dataset.phase = phase;
  $('agent-phase').textContent = title;
  $('agent-detail').textContent = String(detail);
  $('turn-count').textContent = session.mode === 'resource-limit' ? `TURN ${count(session.turn)} / UNTIL LIMIT` : session.maxTurns ? `TURN ${count(session.turn)} / ${count(session.maxTurns)}` : 'AGENT';
  $('tool-count').textContent = agent.maxToolCalls == null ? `${count(agent.toolCalls)} tool calls · no cumulative cap` : `${count(agent.toolCalls)} / ${count(agent.maxToolCalls)} tool calls`;
  $('agent-status').textContent = title;
  $('activity-marker').dataset.active = String(['starting', 'running'].includes(status));
  let activity = agent.error || session.reason || latest?.message || next.experimentDetails?.mission || 'Waiting for the experiment.';
  if (typeof activity !== 'string') activity = '';
  $('activity-text').textContent = activity.replace(/\s+/g, ' ').trim();
  $('activity-text').title = activity;
  $('agent-status').title = `${count(agent.toolCalls)} machine tool calls. A completed turn is not proof of guest task success.`;
}
function renderState(next) {
  state = next;
  const machine = next.machine || {};
  const config = machine.config || next.config || {};
  const details = next.experimentDetails || {};
  if (next.experiment !== lastExperiment) {
    previousBytes.clear(); changedUntil.clear(); lastPc = undefined; lastRenderedAddress = undefined;
    lastExperiment = next.experiment;
    modeDirty = false; controlFeedback('');
    resetJournal(next.experiment);
    lastChangeId = 0; hotRanges = []; selectedChange = null; historySignature = ''; mapSignature = ''; focusAddress = null;
    $('change-kind').textContent = 'AWAITING A WRITE';
    $('change-time').textContent = '—';
    $('change-label').textContent = 'Assembly will appear here when it reaches RAM.';
    $('change-address').textContent = 'No changed address';
    $('change-size').textContent = '—';
    $('byte-diff').hidden = true;
    $('change-history').replaceChildren();
    const empty = document.createElement('p'); empty.className = 'history-empty'; empty.textContent = 'Waiting for the first memory write.';
    $('change-history').append(empty);
    $('memory-heat').replaceChildren();
  }
  updateGoal(journalMission ?? details.mission);
  const name = details.name || 'experiment';
  $('experiment-name').textContent = name;
  document.title = `${name} / picos`;
  $('model').textContent = details.model || next.agent?.model || 'Codex default';
  $('model').title = $('model').textContent;
  $('cpu').textContent = count(Number(config.clockHz || 0) / 1e6);
  $('ram').textContent = mb(config.ramBytes);
  $('storage').textContent = mb(config.diskBytes);
  $('execution-boundary').textContent = next.strict
    ? `Strict · VM tools only · ${machine.sealed ? 'guest sealed' : 'assembly bootstrap'}`
    : 'Codex harness · external orchestration';
  $('execution-boundary').title = next.strict
    ? (machine.sealed ? 'Direct ChatGPT connection. New software must run through guest instructions; external assembly and byte injection are disabled.' : 'Direct ChatGPT connection with no external code execution tools. The fixed cross-assembler and byte loader remain available until the guest is sealed.')
    : 'This saved experiment uses the earlier Codex harness, which permits external orchestration. It is not a strict experiment.';
  $('ram-touched').textContent = byteCount(machine.ramTouchedBytes || 0);
  $('memory-capacity').textContent = `${mb(config.ramBytes)} MB total`;
  $('cycles').textContent = count(machine.cycles);
  $('virtual-time').textContent = `${(Number(machine.cycles || 0) / Number(config.clockHz || 1)).toFixed(4)} s virtual`;
  const machineStatus = next.running ? 'running' : machine.status || 'waiting';
  $('machine-status').textContent = machineStatus.charAt(0).toUpperCase() + machineStatus.slice(1);
  $('machine-light').dataset.state = machineStatus;
  renderMemoryActivity(next);
  renderAgent(next);
  renderSessionControls(next);
  $('status-note').textContent = machine.sealed ? 'Guest bootstrap sealed' : 'Guest read only · session controls enabled';
  if (machine.serial !== lastSerial) {
    const serial = typeof machine.serial === 'string' ? machine.serial : '';
    const terminal = $('serial');
    const atBottom = terminal.scrollTop + terminal.clientHeight >= terminal.scrollHeight - 12;
    terminal.textContent = serial;
    $('serial-empty').hidden = serial.length > 0;
    $('serial-count').textContent = `${count(serial.length)} bytes`;
    if (atBottom) terminal.scrollTop = terminal.scrollHeight;
    lastSerial = serial;
  }
  $('address-go').disabled = !ramBytes();
  if (machine.fault) error(`Guest trap: ${machine.fault}`);
  else if (next.agent?.error) error(`Agent: ${next.agent.error}`);
  else if (!memoryError) error('');
}
function drawFrame(bytes) {
  if (bytes.length !== 256 * 192) throw new Error('The guest framebuffer has an invalid size.');
  for (let index = 0; index < bytes.length; index++) {
    const value = bytes[index];
    pixels.data[index * 4] = Math.round((value >>> 5) * 255 / 7);
    pixels.data[index * 4 + 1] = Math.round(((value >>> 2) & 7) * 255 / 7);
    pixels.data[index * 4 + 2] = (value & 3) * 85;
    pixels.data[index * 4 + 3] = 255;
  }
  context.putImageData(pixels, 0, 0);
}
function renderMemory(result) {
  if (!Array.isArray(result.rows)) throw new Error('The memory response did not contain disassembly rows.');
  const address = Number(result.address);
  const length = Number(result.length);
  const total = Number(result.ramBytes);
  const pc = Number(result.pc);
  const now = performance.now();
  const fragment = document.createDocumentFragment();
  let pcRow;
  let focusRow;
  for (const row of result.rows) {
    const entry = document.createElement('div');
    entry.className = `memory-row${row.isPc ? ' is-pc' : ''}${row.kind === 'data' ? ' is-data' : ''}`;
    const location = document.createElement('span');
    location.className = 'memory-address'; location.textContent = hex(row.address);
    const content = document.createElement('div');
    const instruction = document.createElement('div');
    instruction.className = 'memory-instruction'; instruction.textContent = row.assembly || '';
    const raw = document.createElement('div'); raw.className = 'memory-bytes';
    let changed = false;
    (row.bytes || []).forEach((value, offset) => {
      const byteAddress = Number(row.address) + offset;
      if (previousBytes.has(byteAddress) && previousBytes.get(byteAddress) !== value) changedUntil.set(byteAddress, now + highlightMs);
      previousBytes.set(byteAddress, value);
      const isChanged = (changedUntil.get(byteAddress) || 0) > now || hotRanges.some((range) => byteAddress >= range.start && byteAddress < range.end);
      const byte = document.createElement('span');
      byte.className = `memory-byte${isChanged ? ' changed' : ''}`;
      byte.textContent = value.toString(16).padStart(2, '0');
      byte.title = `RAM 0x${hex(byteAddress)} · ${value}`;
      raw.append(byte); changed ||= isChanged;
    });
    if (changed) entry.classList.add('has-changes');
    if (focusAddress !== null && Number(row.address) <= focusAddress && Number(row.address) + (row.bytes?.length || 8) > focusAddress) focusRow = entry;
    if (row.isPc) { pcRow = entry; entry.setAttribute('aria-label', `Program counter at ${hex(row.address)}: ${row.assembly}`); }
    content.append(instruction, raw); entry.append(location, content); fragment.append(entry);
  }
  const scroller = $('memory-scroll');
  const scrollTop = scroller.scrollTop;
  $('memory-rows').replaceChildren(fragment);
  $('memory-rows').classList.toggle('without-bytes', !showBytes);
  $('memory-empty').hidden = result.rows.length > 0;
  $('memory-empty').textContent = 'No readable bytes in this range.';
  $('view-range').textContent = `${hex(address)} – ${hex(address + Math.max(0, length - 1))}`;
  $('pc-value').textContent = `PC ${hex(pc)}`;
  $('pc-value').title = 'Current byte-addressed instruction pointer';
  $('memory-window').style.left = `${Math.min(100, address / Math.max(1, total) * 100)}%`;
  $('memory-window').style.width = `${Math.max(0, length / Math.max(1, total) * 100)}%`;
  $('memory-window').parentElement.title = `Viewed RAM: ${length} bytes at 0x${hex(address)} of ${count(total)} bytes. This marker is not an allocation map.`;
  if (document.activeElement !== $('address')) $('address').value = `0x${hex(address)}`;
  $('page-previous').disabled = address <= 0;
  $('page-next').disabled = address + length >= total;
  if (followWrites && focusRow && (focusVersion !== lastFocusVersion || address !== lastRenderedAddress)) scroller.scrollTop = Math.max(0, focusRow.offsetTop - 16);
  else if (followPc && pcRow && (pc !== lastPc || address !== lastRenderedAddress)) scroller.scrollTop = Math.max(0, pcRow.offsetTop - scroller.clientHeight / 3);
  else scroller.scrollTop = scrollTop;
  lastPc = pc; lastRenderedAddress = address; lastFocusVersion = focusVersion;
  // This is an observer-side comparison cache, never guest memory or allocation.
  if (previousBytes.size > 4096) {
    for (const key of previousBytes.keys()) if (key < address || key >= address + length) previousBytes.delete(key);
  }
  for (const [key, expiry] of changedUntil) if (expiry <= now) changedUntil.delete(key);
  const registers = document.createDocumentFragment();
  (result.registers || []).forEach((value, index) => {
    const register = document.createElement('div');
    const label = document.createElement('span'); label.textContent = `r${index}${index === 15 ? ' / sp' : ''}`;
    const data = document.createElement('b'); data.textContent = hex(value);
    register.append(label, data); registers.append(register);
  });
  $('registers').replaceChildren(registers);
}
async function refresh() {
  if (accessRequired) return;
  if (requestInFlight) { refreshAgain = true; return; }
  clearTimeout(refreshTimer);
  requestInFlight = true;
  try {
    const [next, frame] = await Promise.all([
      request('/api/state').then((response) => response.json()),
      request('/api/frame').then((response) => response.arrayBuffer()),
    ]);
    renderState(next);
    drawFrame(new Uint8Array(frame));
    connection(true);
    const version = navigationVersion;
    if (followPc) {
      const pc = Number(next.machine?.pc || 0);
      // The ISA is byte-addressed: retain PC's alignment even near RAM address 0.
      viewAddress = clampAddress(pc - Math.min(6, Math.floor(pc / 8)) * 8);
    }
    else viewAddress = clampAddress(viewAddress);
    const length = Math.min(pageBytes, ramBytes() - viewAddress);
    try {
      if (length < 8) throw new Error('No inspectable RAM is available.');
      const result = await request(`/api/disassembly?address=${viewAddress}&length=${length}`).then((response) => response.json());
      if (version === navigationVersion) { memoryError = ''; renderMemory(result); }
    } catch (failure) {
      memoryError = failure.message;
      error(`Memory observer: ${failure.message}`);
    }
  } catch (failure) {
    connection(false);
    if (!accessRequired) error(`Observer disconnected: ${failure.message}`);
  } finally {
    requestInFlight = false;
    if (!accessRequired) refreshTimer = setTimeout(refresh, refreshAgain ? 0 : document.hidden ? 5000 : 750);
    refreshAgain = false;
  }
}
function navigate(address) {
  setFollow('manual'); viewAddress = clampAddress(address); navigationVersion++;
  $('memory-scroll').scrollTop = 0;
  void refresh();
}
$('address-form').addEventListener('submit', (event) => {
  event.preventDefault();
  const value = $('address').value.trim();
  if (!/^(?:0x[0-9a-f]+|\d+)$/i.test(value)) { error('Enter a decimal or hexadecimal guest RAM address.'); return; }
  const address = Number(value);
  if (!Number.isSafeInteger(address) || address < 0 || address >= ramBytes()) { error(`Address must be within RAM: 0 through ${count(Math.max(0, ramBytes() - 1))}.`); return; }
  error(''); navigate(address);
});
$('follow-pc').addEventListener('click', () => { setFollow(followPc ? 'manual' : 'pc'); navigationVersion++; void refresh(); });
$('follow-writes').addEventListener('click', () => {
  setFollow(followWrites ? 'manual' : 'writes');
  if (followWrites && memoryEvents.length) selectChange(memoryEvents.at(-1), true);
  navigationVersion++; void refresh();
});
$('page-previous').addEventListener('click', () => navigate(viewAddress - pageBytes));
$('page-next').addEventListener('click', () => navigate(viewAddress + pageBytes));
$('show-bytes').addEventListener('click', () => {
  showBytes = !showBytes;
  $('show-bytes').setAttribute('aria-pressed', String(showBytes));
  $('memory-rows').classList.toggle('without-bytes', !showBytes);
});
$('serial-drawer').addEventListener('toggle', () => { if ($('serial-drawer').open) $('serial').scrollTop = $('serial').scrollHeight; });
$('run-mode').addEventListener('change', () => { modeDirty = true; renderSessionControls(state || {}); });
$('apply-mode').addEventListener('click', () => {
  if ($('apply-mode').disabled) return;
  void sessionControl('/api/session/settings', { mode: $('run-mode').value }, 'Mode applied. Start or continue when you are ready.');
});
$('tell-turns').addEventListener('change', () => {
  if ($('tell-turns').disabled) return;
  void sessionControl('/api/session/settings', { tellTurns: $('tell-turns').checked }, 'Turn-budget visibility updated for the next turn.');
});
$('grant-turns').addEventListener('submit', (event) => {
  event.preventDefault();
  if ($('add-turns').disabled) return;
  const input = $('additional-turns');
  const turns = Number(input.value);
  if (!Number.isSafeInteger(turns) || turns < 1 || turns > Number(input.max)) {
    controlFeedback(`Choose a whole number from 1 to ${input.max}.`, true); return;
  }
  void sessionControl('/api/session/turns', { turns }, `${count(turns)} additional turn${turns === 1 ? '' : 's'} granted.`);
});
$('continue-session').addEventListener('click', () => {
  if ($('continue-session').disabled) return;
  void sessionControl('/api/session/continue', {}, 'Experiment continuation requested.');
});
$('stop-session').addEventListener('click', () => {
  if ($('stop-session').disabled) return;
  void sessionControl('/api/session/stop', {}, 'Stop requested. Machine state is preserved.');
});
$('memory-tab').addEventListener('click', () => setInspector('memory'));
$('journal-tab').addEventListener('click', () => setInspector('journal'));
$('open-journal').addEventListener('click', () => { setInspector('journal'); if (!phoneLayout.matches) $('journal-tab').focus(); });
for (const [id, other] of [['memory-tab', 'journal-tab'], ['journal-tab', 'memory-tab']]) $(id).addEventListener('keydown', (event) => {
  if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) {
    event.preventDefault(); const target = event.key === 'Home' ? 'memory-tab' : event.key === 'End' ? 'journal-tab' : other;
    setInspector(target === 'journal-tab' ? 'journal' : 'memory'); $(target).focus();
  }
});
$('journal-filter').addEventListener('change', () => { journalFilter = $('journal-filter').value; renderJournalEntries(true); });
$('journal-follow').addEventListener('click', () => { setJournalFollow(!journalFollowing); if (journalFollowing) scrollJournalBottom(); });
$('journal-scroll').addEventListener('scroll', () => {
  const scroller = $('journal-scroll');
  if (journalActive && journalFollowing && scroller.scrollTop + scroller.clientHeight < scroller.scrollHeight - 24) setJournalFollow(false);
}, { passive: true });
$('copy-goal').addEventListener('click', async () => {
  const text = $('goal-text').textContent;
  try {
    if (navigator.clipboard && window.isSecureContext) await navigator.clipboard.writeText(text);
    else {
      // HTTP on a private LAN has no Clipboard API. Keep the copy action usable
      // through the browser's user-initiated legacy clipboard path.
      const field = document.createElement('textarea');
      field.value = text; field.readOnly = true;
      field.style.cssText = 'position:fixed;top:-1000px;left:0;';
      document.body.append(field); field.select(); field.setSelectionRange(0, text.length);
      const copied = document.execCommand('copy'); field.remove();
      if (!copied) throw new Error('Clipboard unavailable');
    }
    $('goal-copy-status').textContent = 'Goal copied.';
  } catch { $('goal-copy-status').textContent = 'Copy unavailable. Press and hold the goal text to copy it.'; }
});
for (const button of document.querySelectorAll('.mobile-nav button')) button.addEventListener('click', () => setMobileView(button.dataset.view));
$('access-form').addEventListener('submit', (event) => {
  event.preventDefault();
  let key = $('access-key').value.trim();
  try { key = new URLSearchParams(new URL(key).hash.slice(1)).get('access') || key; } catch {}
  if (!/^[a-f0-9]{64}$/i.test(key)) { $('access-feedback').textContent = 'Paste the complete private link or its 64-character access key.'; return; }
  accessKey = key;
  try { sessionStorage.setItem('picos-access', key); } catch {}
  accessRequired = false; delete document.body.dataset.accessRequired;
  $('access-panel').hidden = true; $('access-key').value = '';
  void refresh(); void refreshJournal();
});
document.addEventListener('visibilitychange', () => { if (!document.hidden) { void refresh(); void refreshJournal(); } });
setInspector('journal', false);
if (phoneLayout.matches) setMobileView('display');
void refresh();
void refreshJournal();
