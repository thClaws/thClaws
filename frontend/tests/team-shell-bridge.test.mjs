import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

function bridge() {
  const listeners = {}, sent = [];
  const window = {addEventListener(type, callback) { (listeners[type] ||= []).push(callback); }};
  vm.runInNewContext(readFileSync(new URL('../../crates/core/assets/gui-shell-bridge.js', import.meta.url), 'utf8'), {
    window, location: {href:'http://localhost/gui-shell/team-control/index.html?session=tier1'}, URL,
    parent: {postMessage(frame) { sent.push(frame); }},
    setTimeout: () => 1, clearTimeout() {}, console,
  });
  return {api:window.thclaws.team,sent,reply(frame) { listeners.message.forEach(cb => cb({data:{ns:'thclaws-shell-event',...frame}})); }};
}

test('team bridge commands carry explicit agent/session and never use shared run or cancel', async () => {
  const {api,sent,reply} = bridge();
  const requests = [api.snapshot(), api.message('writer','writer-session','write this'), api.stop('researcher','research-session'), api.history('lead','lead-session')];
  const frames = sent.filter(f => f.type === 'team');
  assert.equal(frames.length,4);
  assert.equal(frames[1].payload.agent,'writer');
  assert.equal(frames[1].payload.targetSession,'writer-session');
  assert.equal(frames[2].payload.agent,'researcher');
  assert.equal(frames[2].payload.targetSession,'research-session');
  assert.ok(!sent.some(f => ['run','cancel','session_load','session_new'].includes(f.type)));
  for(const f of frames) reply({shellId:'team-control',replyTo:f.requestId,result:{ok:true}});
  await Promise.all(requests);
});

test('team reply cannot resolve another shell request with the same numeric ID', async () => {
  const {api,sent,reply} = bridge();
  let resolved = false;
  const promise = api.snapshot().then(result => {resolved = true; return result;});
  const id = sent.at(-1).requestId;
  reply({shellId:'chatbot',replyTo:id,result:{wrong:true}});
  await Promise.resolve();
  assert.equal(resolved,false);
  reply({shellId:'team-control',replyTo:id,result:{agents:[]}});
  assert.deepEqual(await promise,{agents:[]});
});
