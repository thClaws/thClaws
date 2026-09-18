import { useEffect, useState } from "react";
import { send, subscribe } from "../hooks/useIPC";

type Member = { name: string; role: string; prompt: string };
type Team = { name: string; description?: string; members: Member[] };
type Status = { name: string; status: string; alive: boolean };
type Editor = { action: string; name: string; description: string; role: string; prompt: string };

export function TeamManager() {
  const [team, setTeam] = useState<Team | null>(null);
  const [statuses, setStatuses] = useState<Status[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [open, setOpen] = useState(false);
  const [editor, setEditor] = useState<Editor | null>(null);
  const [pending, setPending] = useState<string | null>(null);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);
  useEffect(() => subscribe((msg) => {
    if (msg.type === "team_status") {
      setTeam((msg.team as Team | null) ?? null);
      setStatuses((msg.agents as Status[]) ?? []);
      setLoaded(true);
    }
    if (msg.type === "team_manage_result" && msg.request_id === pending) {
      setPending(null);
      setNotice({ ok: msg.ok === true, text: String(msg.text) });
      if (msg.ok) setEditor(null);
      send({ type: "team_list" });
    }
  }), [pending]);
  const act = (action: string, values: Record<string, unknown> = {}) => {
    const id = crypto.randomUUID();
    setPending(id);
    setNotice(null);
    send({ type: "team_manage", action, request_id: id, ...values });
  };
  const edit = (action: string, member?: Member) => {
    setNotice(null);
    setEditor({ action, name: member?.name ?? (action === "edit_team" ? team?.name ?? "" : ""), description: team?.description ?? "", role: member?.role ?? "", prompt: member?.prompt ?? "" });
  };
  const button = "px-2 py-1 rounded border border-[var(--border)] text-xs disabled:opacity-40 hover:bg-[var(--bg-tertiary)]";
  const input = "w-full mt-1 rounded border border-[var(--border)] bg-[var(--bg-primary)] p-2 text-sm";
  const memberForm = editor?.action.endsWith("member");
  return <div className="border-b border-[var(--border)] px-3 py-2 text-xs">
    <div className="flex items-center gap-2 flex-wrap">
      <strong>{team?.name ?? "No team configured"}</strong>
      <span className="text-[var(--text-secondary)]">Team within this bot</span>
      <button className={button} disabled={!loaded} onClick={() => setOpen(!open)}>{open ? "Hide management" : "Manage team"}</button>
    </div>
    {open && <div className="mt-3 max-h-[55vh] overflow-y-auto space-y-3">
      <p className="text-[var(--text-secondary)]">The lead belongs to this bot. Add teammates here to run separate agents. Removing a member keeps its sessions and history.</p>
      <div className="flex gap-2 flex-wrap">
        <button className={button} disabled={!!pending} onClick={() => edit(team ? "edit_team" : "create")}>{team ? "Edit team" : "Create team"}</button>
        {team && <>
          <button className={button} disabled={!!pending} onClick={() => edit("add_member")}>Add teammate</button>
          <button className={button} disabled={!!pending || !team.members.length} onClick={() => act("shutdown_all")}>Request shutdown of all teammates</button>
        </>}
      </div>
      {team?.description && <p>{team.description}</p>}
      {team?.members.map(member => {
        const status = statuses.find(s => s.name === member.name);
        const live = !!status && status.alive && status.status !== "stopped";
        return <div key={member.name} className="border border-[var(--border)] rounded p-2 flex items-center gap-2 flex-wrap">
          <span className="flex-1 min-w-32"><strong>{member.name}</strong> · {member.role || "No role"}<br /><span className="text-[var(--text-secondary)]">{status ? (live ? status.status : `${status.status} / offline`) : "Not started"}</span></span>
          <button className={button} disabled={!!pending || live} onClick={() => edit("edit_member",member)}>Edit</button>
          <button className={button} disabled={!!pending || live} onClick={() => act("start_member", {name:member.name})}>Start</button>
          <button className={button} disabled={!!pending || !live || status?.status !== "working"} onClick={() => act("abort_member", {name:member.name})}>Stop turn</button>
          <button className={button} disabled={!!pending || !live} onClick={() => act("shutdown_member", {name:member.name})}>Request shutdown</button>
          <button className={button} disabled={!!pending || live} onClick={() => act("remove_member", {name:member.name})}>Remove member</button>
        </div>;
      })}
      {editor && <form className="rounded border border-[var(--border)] p-3 space-y-2" onSubmit={e => {e.preventDefault(); const {action,...values}=editor; act(action,values);}}>
        <h3 className="font-semibold">{memberForm ? "Teammate" : "Team"} details</h3>
        <label className="block">{memberForm ? "Teammate name" : "Team name"}<input className={input} required maxLength={memberForm ? 64 : 120} pattern={memberForm ? "[A-Za-z0-9_][A-Za-z0-9_-]*" : undefined} readOnly={editor.action === "edit_member"} value={editor.name} onChange={e => setEditor({...editor,name:e.target.value})} /></label>
        {memberForm ? <>
          <label className="block">Role<input className={input} value={editor.role} onChange={e => setEditor({...editor,role:e.target.value})} /></label>
          <label className="block">Instructions / initial task<textarea className={input} required rows={3} value={editor.prompt} onChange={e => setEditor({...editor,prompt:e.target.value})} /></label>
          <p className="text-[var(--text-secondary)]">Shared workspace. Start launches a teammate using the bot's configured model and saved instructions. Editing applies on its next start.</p>
        </> : <label className="block">Team goal<textarea className={input} rows={2} value={editor.description} onChange={e => setEditor({...editor,description:e.target.value})} /></label>}
        <div className="flex gap-2"><button type="submit" className={button} disabled={!!pending}>Save</button><button type="button" className={button} disabled={!!pending} onClick={() => setEditor(null)}>Cancel</button></div>
      </form>}
      {pending && <p role="status">Applying change…</p>}
      {notice && <p role={notice.ok ? "status" : "alert"} style={{color:notice.ok ? "var(--text-primary)" : "var(--danger)"}}>{notice.text}</p>}
    </div>}
  </div>;
}
