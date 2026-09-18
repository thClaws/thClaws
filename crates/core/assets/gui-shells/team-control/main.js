(() => {
  const api = window.thclaws.team;
  const $ = (id) => document.getElementById(id);
  let snapshot = null, selected = 'lead', pending = false, polling = false, editing = false;
  let listSignature = '', historyTarget = '';
  const drafts = new Map();
  function notice(text, error = false) { $('notice').textContent = text; $('notice').classList.toggle('error', error); }
  function current() { return snapshot?.agents.find(a => a.name === selected); }
  function target() {
    const agent = current();
    if (!agent?.session_id) throw new Error('This agent has no session yet. Start it first.');
    return { name: agent.name, session: agent.session_id };
  }
  async function action(work) {
    if (pending) return;
    pending = true; render();
    try { const result = await work(); if (result?.text) notice(result.text); }
    catch (error) { notice(error.message || String(error), true); }
    finally { pending = false; await refresh(); render(); }
  }
  function render() {
    if (!snapshot) return;
    const team = snapshot.team;
    $('team-name').textContent = team?.name || 'Create your team';
    $('team-goal').textContent = team?.description || 'One team per bot. Each teammate runs independently.';
    $('edit-team').textContent = team ? 'Edit team' : 'Create team';
    $('edit-team').disabled = pending;
    $('add-member').disabled = pending || !team;
    $('count').textContent = `(${snapshot.agents.length})`;
    const signature = JSON.stringify(snapshot.agents.map(a => [a.name,a.role,a.status,a.session_id])) + selected;
    if (signature !== listSignature) {
      listSignature = signature;
      $('agents').replaceChildren(...snapshot.agents.map(agent => {
        const button = document.createElement('button');
        button.setAttribute('aria-current', String(selected === agent.name));
        const name = document.createElement('strong'); name.textContent = agent.name;
        const detail = document.createElement('small'); detail.textContent = `${agent.role || 'Teammate'} · ${agent.status}`;
        button.append(name, detail);
        button.onclick = () => {
          drafts.set(selected, $('message').value);
          selected = agent.name; $('message').value = drafts.get(selected) || '';
          $('history-content').replaceChildren(); historyTarget = ''; render();
        };
        return button;
      }));
    }
    const agent = current(); if (!agent) return;
    if (historyTarget && historyTarget !== `${agent.name}:${agent.session_id}`) {
      $('history-content').replaceChildren(); historyTarget = '';
    }
    $('agent-name').textContent = agent.name;
    $('agent-role').textContent = agent.role || 'Teammate';
    $('agent-status').textContent = agent.status;
    $('agent-session').textContent = agent.session_id || 'No session — start this agent to begin.';
    $('progress').textContent = agent.progress || (agent.status === 'working' ? 'Working…' : agent.live ? 'Ready for instructions' : 'Worker is stopped or disconnected');
    const activity = agent.output.join('\n') || 'No worker output yet.';
    if ($('activity').textContent !== activity) { $('activity').textContent = activity; $('activity').scrollTop = $('activity').scrollHeight; }
    const lead = agent.name === 'lead';
    $('start').hidden = lead; $('start').disabled = pending || agent.live;
    $('stop').disabled = pending || !agent.live || agent.status !== 'working';
    for (const id of ['shutdown','edit-member','remove']) $(id).hidden = lead;
    $('shutdown').disabled = pending || !agent.live;
    $('edit-member').disabled = pending || agent.live;
    $('remove').disabled = pending || agent.live;
    $('history').disabled = pending || !agent.session_id;
    $('message-label').textContent = `Instructions for ${agent.name}`;
    $('message').disabled = pending || !agent.live || !agent.session_id;
    $('send').disabled = $('message').disabled;
    document.querySelectorAll('#save-team, #save-member, #send').forEach(button => button.disabled = pending || (button.id === 'send' && (!agent.live || !agent.session_id)));
  }
  async function refresh() {
    if (polling) return;
    polling = true;
    try {
      snapshot = await api.snapshot();
      if (!snapshot.agents.some(a => a.name === selected)) selected = 'lead';
      render();
    } catch(error) { notice(`Could not refresh team: ${error.message}`, true); }
    finally { polling = false; }
  }
  $('refresh').onclick = refresh;
  $('fullscreen').onclick = () => window.thclaws.ui.toggleFullscreen();
  window.thclaws.ui.onFullscreen(active => { $('fullscreen').textContent = active ? 'Exit full screen' : 'Full screen'; });
  $('edit-team').onclick = () => {
    const form = $('team-form'); form.hidden = false;
    form.elements.name.value = snapshot?.team?.name || '';
    form.elements.description.value = snapshot?.team?.description || '';
    form.elements.name.focus();
  };
  $('save-team').onclick = () => {
    const form = $('team-form'); if (!form.reportValidity()) return;
    const operation = {action:snapshot.team ? 'edit_team' : 'create',name:form.elements.name.value,description:form.elements.description.value};
    action(async () => { const result = await api.manage(operation); form.hidden = true; return result; });
  };
  function memberForm(edit) {
    editing = edit; const form = $('member-form'); form.reset(); form.hidden = false;
    $('member-form-title').textContent = edit ? 'Edit agent' : 'Add agent';
    const member = edit ? snapshot.team.members.find(m => m.name === selected) : null;
    for (const field of ['name','role','prompt']) form.elements[field].value = member?.[field] || '';
    form.elements.name.readOnly = edit; form.elements[edit ? 'role' : 'name'].focus();
  }
  $('add-member').onclick = () => memberForm(false);
  $('edit-member').onclick = () => memberForm(true);
  $('save-member').onclick = () => {
    const form = $('member-form'); if (!form.reportValidity()) return;
    const operation = {action:editing ? 'edit_member' : 'add_member'};
    for (const field of ['name','role','prompt']) operation[field] = form.elements[field].value;
    action(async () => { const result = await api.manage(operation); form.hidden = true; selected = operation.name; return result; });
  };
  document.querySelectorAll('[data-close]').forEach(button => { button.onclick = () => $(button.dataset.close).hidden = true; });
  for (const [id, operation] of [['start','start_member'],['shutdown','shutdown_member'],['remove','remove_member']]) {
    $(id).onclick = () => { const name = selected; action(() => api.manage({action:operation,name})); };
  }
  $('stop').onclick = () => { const t = target(); action(() => api.stop(t.name,t.session)); };
  $('send').onclick = () => {
    if (!$('message-form').reportValidity()) return;
    const t = target(), text = $('message').value;
    action(async () => {
      const result = await api.message(t.name,t.session,text);
      drafts.delete(t.name); if (selected === t.name) $('message').value = '';
      return result;
    });
  };
  $('history').onclick = () => {
    const t = target();
    action(async () => {
      const history = await api.history(t.name,t.session);
      if (current()?.name !== t.name || current()?.session_id !== t.session) return;
      historyTarget = `${t.name}:${t.session}`;
      const rows = history.messages.map(message => {
        const row = document.createElement('div'); row.className = 'history-row';
        const role = document.createElement('strong'); role.textContent = message.role;
        row.append(role, document.createTextNode(message.text || '')); return row;
      });
      if (!rows.length) { const empty = document.createElement('p'); empty.textContent = 'No completed turns saved yet.'; rows.push(empty); }
      $('history-content').replaceChildren(...rows);
    });
  };
  notice('Select an agent to assign work. Agents can run concurrently.');
  refresh(); setInterval(() => { if (!document.hidden) refresh(); }, 2000);
})();
