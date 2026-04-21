/* Dyson — TopBar / LeftRail / MindView / ActivityView. */

const { useState: vUS, useEffect: vUE } = React;

function TopBar({ view, setView, onToggleLeft, onToggleRight, rightHidden }) {
  const navs = [
    { id: 'conv',     name: 'Conversations', k: '1', icon: 'chat' },
    { id: 'mind',     name: 'Mind',          k: '2', icon: 'brain' },
    { id: 'activity', name: 'Activity',      k: '3', icon: 'activity' },
  ];
  const D = window.DYSON_DATA || {};
  const model = D.activeModel || '';
  const providers = D.providers || [];
  const totalModels = providers.reduce((n, p) => n + ((p.models && p.models.length) || 0), 0);

  const [menuOpen, setMenuOpen] = vUS(false);
  const [busy, setBusy] = vUS(false);
  // expanded[providerId] === true → group is open.  Active provider
  // starts open, others collapsed.  Resets each time the menu opens so
  // the initial render matches the current active provider.
  const [expanded, setExpanded] = vUS({});
  vUE(() => {
    if (!menuOpen) return;
    const init = {};
    for (const p of providers) init[p.id] = !!p.active;
    setExpanded(init);
  }, [menuOpen]);

  const toggle = (id) => setExpanded(e => ({ ...e, [id]: !e[id] }));

  const switchTo = async (provider, modelName) => {
    if (!window.DysonLive) { setMenuOpen(false); return; }
    setBusy(true);
    try {
      const r = await fetch('/api/model', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ provider, model: modelName }),
      });
      if (r.ok) {
        D.activeModel = modelName;
        D.providers = providers.map(p => ({
          ...p,
          active: p.id === provider,
          activeModel: p.id === provider ? modelName : p.activeModel,
        }));
        window.dispatchEvent(new CustomEvent('dyson:live-update'));
      }
    } catch (e) { console.error(e); }
    setBusy(false);
    setMenuOpen(false);
  };

  return (
    <div className="topbar">
      <button className="menu-toggle" title="Conversations" onClick={onToggleLeft}><Icon name="menu" size={14}/></button>
      <div className="brand"><div className="mark">D</div><div className="name">Dyson</div></div>
      <nav>
        {navs.map(n => (
          <button key={n.id} className={view === n.id ? 'active' : ''} onClick={() => setView(n.id)}>
            <Icon name={n.icon} size={13}/> <span>{n.name}</span> <span className="k">⌘{n.k}</span>
          </button>
        ))}
      </nav>
      <div className="spacer"/>
      <div className="meta" style={{position:'relative'}}>
        {model && (
          <span className="select" onClick={() => totalModels > 0 && setMenuOpen(o => !o)}
                style={{cursor: totalModels > 0 ? 'pointer' : 'default', opacity: busy ? 0.5 : 1}}
                title={totalModels > 0 ? 'Switch model' : 'Active model'}>
            <span className="label">model</span> <span className="mono">{model}</span>
            {totalModels > 0 && <Icon name="chevd" size={10}/>}
          </span>
        )}
        <button className={`menu-toggle ${!rightHidden ? 'active' : ''}`}
                title={rightHidden ? 'Show tool stack' : 'Hide tool stack'}
                onClick={onToggleRight}>
          <Icon name="plug" size={14}/>
        </button>
        {menuOpen && totalModels > 0 && (
          <>
            <div className="modelmenu-scrim" onClick={() => setMenuOpen(false)}/>
            <div className="modelmenu">
              {providers.map(p => {
                const open = !!expanded[p.id];
                const models = p.models || [];
                return (
                  <div key={p.id} className={`group ${p.active ? 'active' : ''}`}>
                    <div className="g-head" onClick={() => toggle(p.id)}>
                      <span className="caret" style={{transform: open ? 'rotate(90deg)' : 'none'}}>
                        <Icon name="chev" size={10}/>
                      </span>
                      <span className="name">{p.name}</span>
                      {p.active && <span className="badge">active</span>}
                      <span className="count">{models.length}</span>
                    </div>
                    {open && models.length > 0 && (
                      <div className="g-body">
                        {models.map(m => (
                          <div key={m}
                               className={`item ${(p.active && m === model) ? 'on' : ''}`}
                               onClick={() => switchTo(p.id, m)}>
                            <span className="dot"/>
                            <span className="model mono">{m}</span>
                          </div>
                        ))}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function LeftRail({ active, setActive }) {
  // Chat history is shared across controllers; one flat list is the
  // accurate shape (Telegram-originated and HTTP-originated chats both
  // live in ~/.dyson/chats and the controller has no honest way to
  // attribute origin without metadata that doesn't exist yet).
  const items = (window.DYSON_DATA.conversations.http) || [];
  const newConv = () => {
    if (!window.DysonLive) return;
    window.DysonLive.createChat('New conversation').then(c => {
      window.DYSON_DATA.conversations.http.unshift({ id: c.id, title: c.title, live: false });
      setActive(c.id);
    });
  };
  return (
    <aside className="left">
      <div className="newc">
        <button className="btn primary" onClick={newConv}>
          <span><Icon name="plus" size={12}/> New conversation</span>
          <Kbd>⌘N</Kbd>
        </button>
      </div>
      <div className="search"><input placeholder="Filter conversations"/></div>
      <div className="scroll">
        {items.length === 0 ? (
          <div style={{padding:'18px 14px', color:'var(--mute)', fontSize:12, lineHeight:1.5}}>
            No conversations yet. <span className="mono" style={{color:'var(--fg-dim)'}}>⌘N</span> to start one.
          </div>
        ) : (
          <div className="group">
            <h4>Conversations <span className="n">· {items.length}</span></h4>
            {items.map(c => (
              <div key={c.id} className={`conv ${c.live ? 'live' : ''} ${active === c.id ? 'active' : ''}`}
                   onClick={() => setActive(c.id)}>
                <div className="row1"><span className="title">{c.title || c.id}</span></div>
                <div className="row2">
                  <span className="last mono" style={{fontSize:10.5, opacity:0.6}}>{c.id}</span>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </aside>
  );
}

function MindView() {
  const m = window.DYSON_DATA.mind;
  const initial = (m.files[0] && m.files[0].path) || '';
  const [selected, setSelected] = vUS(initial);
  const [loaded, setLoaded] = vUS('');
  const [draft, setDraft] = vUS('');
  const [saving, setSaving] = vUS(false);
  const [err, setErr] = vUS('');

  vUE(() => {
    if (!selected) { setLoaded(''); setDraft(''); return; }
    if (window.DysonLive) {
      window.DysonLive.mindFile(selected)
        .then(file => { const c = file.content || ''; setLoaded(c); setDraft(c); setErr(''); })
        .catch(e => { setErr(String(e.message || e)); setLoaded(''); setDraft(''); });
    }
  }, [selected]);

  const dirty = draft !== loaded;
  const save = async () => {
    if (!selected || !window.DysonLive) return;
    setSaving(true); setErr('');
    try {
      const r = await fetch('/api/mind/file', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path: selected, content: draft }),
      });
      if (!r.ok) throw new Error('save failed: ' + r.status);
      setLoaded(draft);
    } catch (e) { setErr(String(e.message || e)); }
    setSaving(false);
  };

  vUE(() => {
    const h = (e) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 's') {
        e.preventDefault();
        if (dirty && !saving) save();
      }
    };
    window.addEventListener('keydown', h);
    return () => window.removeEventListener('keydown', h);
  }, [dirty, saving, draft, selected]);

  return (
    <div className="mind">
      <aside className="mind-side">
        <div style={{padding:'10px 14px', borderBottom:'1px solid var(--line)'}}>
          <div className="eyebrow">workspace</div>
          {m.backend && <div style={{fontSize:13, color:'var(--fg)', marginTop:4}}><span className="mono">{m.backend}</span> backend</div>}
        </div>
        <div style={{overflowY:'auto', flex:1, padding:'6px 0'}}>
          {(m.files.length === 0) && <div style={{padding:'14px', color:'var(--mute)', fontSize:12}}>No workspace files.</div>}
          {m.files.map(f => (
            <div key={f.path} onClick={() => setSelected(f.path)}
                 style={{display:'flex', alignItems:'center', gap:8, padding:'6px 14px', cursor:'pointer',
                         background: selected === f.path ? 'var(--panel)' : 'transparent',
                         borderLeft: selected === f.path ? '2px solid var(--accent)' : '2px solid transparent'}}>
              <Icon name="file" size={11} style={{color:'var(--mute)'}}/>
              <span className="mono" style={{fontSize:12, color:'var(--fg-dim)', flex:1, whiteSpace:'nowrap', overflow:'hidden', textOverflow:'ellipsis'}}>{f.path}</span>
              {f.size && <span className="mono" style={{fontSize:10, color:'var(--mute)'}}>{f.size}</span>}
            </div>
          ))}
        </div>
      </aside>
      <section className="mind-pane">
        <div style={{display:'flex', alignItems:'center', gap:10, padding:'10px 18px', borderBottom:'1px solid var(--line)', background:'var(--bg)', flexWrap:'wrap'}}>
          <span className="mono" style={{fontSize:13, color:'var(--fg)'}}>{selected || '—'}</span>
          {dirty && <span className="chip" style={{color:'var(--warn)'}}>unsaved</span>}
          {err && <span className="chip" style={{color:'var(--err)'}}>{err}</span>}
          <span style={{flex:1}}/>
          {dirty && <button className="btn sm ghost" onClick={() => setDraft(loaded)} disabled={saving}>revert</button>}
          <button className="btn sm primary" onClick={save} disabled={!dirty || saving || !selected || !window.DysonLive}>
            {saving ? 'saving…' : 'save'} <Kbd>⌘S</Kbd>
          </button>
        </div>
        <textarea className="mind-editor"
          value={draft}
          onChange={e => setDraft(e.target.value)}
          placeholder={selected ? '(empty)' : 'select a file to edit'}
          spellCheck={false}
          disabled={!selected}/>
      </section>
    </div>
  );
}

function RightRail({ panels, onClose }) {
  const tools = window.DYSON_DATA.tools || {};
  return (
    <aside className="right">
      <div className="r-head">
        <span className="title">Tool stack</span>
        <span className="count">{panels.length}</span>
        <div className="spacer"/>
      </div>
      <div className="r-stack">
        {panels.length === 0 && (
          <div style={{color:'var(--mute)', fontSize:12, padding:24, textAlign:'center', lineHeight:1.5}}>
            Tool panels appear here when Dyson runs tools.
            Click <span className="mono">[open]</span> on a tool chip in the transcript.
          </div>
        )}
        {panels.map(ref => {
          const t = tools[ref];
          if (!t) return null;
          return <ToolPanel key={ref} tool={t} onClose={() => onClose(ref)}/>;
        })}
      </div>
    </aside>
  );
}

function ActivityView() {
  const lanes = (window.DYSON_DATA.activity) || [];
  const running = lanes.filter(a => a.status === 'running').length;
  const grouped = ['loop','dream','swarm']
    .map(lane => ({ lane, items: lanes.filter(a => a.lane === lane) }))
    .filter(g => g.items.length > 0);
  return (
    <div style={{flex:1, overflowY:'auto', padding:'22px 32px', background:'var(--bg-1)'}}>
      <div style={{maxWidth: 980, margin:'0 auto'}}>
        <div className="eyebrow" style={{marginBottom:12}}>
          Background lanes{running > 0 && ` · ${running} running`}
        </div>
        {grouped.length === 0 && (
          <div style={{color:'var(--mute)', fontSize:13, padding:'18px 0'}}>
            No background agents, dreams, or swarm tasks running.
          </div>
        )}
        {grouped.map(({ lane, items }) => {
          const label = lane === 'loop' ? 'Loops · recurring'
                     : lane === 'dream' ? 'Dreams · background compaction'
                     : 'Swarm · parallel tasks';
          return (
            <div key={lane} style={{marginBottom:22}}>
              <h4 className="eyebrow" style={{margin:'0 0 8px'}}>{label}</h4>
              <div style={{display:'flex', flexDirection:'column', gap:6}}>
                {items.map((a, i) => (
                  <div key={i} style={{display:'flex', alignItems:'center', gap:14, padding:'10px 14px', background:'var(--bg)', border:'1px solid var(--line)', borderRadius:6}}>
                    <span style={{width:6, height:6, borderRadius:'50%',
                                  background: a.status === 'running' ? 'var(--accent)' : a.status === 'ok' ? 'var(--ok)' : 'var(--mute)',
                                  animation: a.status === 'running' ? 'pulse 1.4s infinite' : ''}}/>
                    <span className="mono" style={{fontSize:12.5, color:'var(--fg)', minWidth:200}}>{a.name}</span>
                    <span style={{fontSize:12.5, color:'var(--fg-dim)', flex:1}}>{a.note}</span>
                    {a.last && <span className="mono" style={{fontSize:11, color:'var(--mute-2)'}}>{a.last}</span>}
                  </div>
                ))}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

// Each component file is wrapped in its own IIFE by Babel-in-browser,
// so cross-file references must be hung off `window`.
Object.assign(window, { TopBar, LeftRail, RightRail, MindView, ActivityView });
