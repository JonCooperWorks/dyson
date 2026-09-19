import React from 'react';
import { Icon } from './icons.jsx';

export function RunControls({ run, running, pending, onAction }) {
  if (!run || run.state?.state === 'finished') return null;
  const waiting = run.state?.state === 'waiting_for_input';
  const title = pending === 'pause' ? 'Pausing after the current step' : waiting
    ? (run.answer_received ? 'Your answer is saved' : 'Your input is needed')
    : running ? 'Work in progress' : 'Ready when you are';
  const detail = waiting
    ? (run.answer_received ? 'Resume to continue with your answer.' : run.state.request?.question || 'Answer the open question to continue.')
    : running ? 'You can pause here and come back later.' : 'Your progress is saved. Continue where you left off.';
  return <section className={`run-controls ${waiting ? 'waiting' : ''}`} aria-label="Run controls">
    <span className="run-state-icon"><Icon name={waiting ? 'chat' : running ? 'activity' : 'play'} size={18}/></span>
    <div className="run-state-copy" role="status"><strong>{title}</strong><span>{detail}</span></div>
    <div className="run-actions">
      {running && <button className="btn sm" disabled={!!pending} onClick={() => onAction('pause')}>{pending === 'pause' ? 'Pausing…' : 'Pause'}</button>}
      {!running && (!waiting || run.answer_received) && <button className="btn primary sm" disabled={!!pending} onClick={() => onAction('resume')}><Icon name="play" size={12}/>{pending === 'resume' ? 'Resuming…' : 'Resume'}</button>}
      {!running && <button className="btn ghost sm" disabled={!!pending} onClick={() => onAction('cancel')}>{pending === 'cancel' ? 'Cancelling…' : 'Cancel run'}</button>}
    </div>
  </section>;
}
