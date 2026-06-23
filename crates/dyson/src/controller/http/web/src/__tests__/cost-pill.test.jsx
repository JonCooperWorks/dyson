import React from 'react';
import { describe, it, expect, afterEach } from 'vitest';
import { render, cleanup } from '@testing-library/react';
import { Turn } from '../components/turns.jsx';

afterEach(() => cleanup());

const assistantTurn = (cost) => ({
  role: 'agent',
  ts: '12:00:00',
  blocks: [{ type: 'text', text: 'Done.' }],
  cost,
});

describe('assistant message cost pill', () => {
  it('renders a compact price and details when display cost is known', () => {
    const { container } = render(
      <Turn
        turn={assistantTurn({
          swarm_llm_audit_id: 42,
          display_cost_usd: 0.0031,
          provider: 'openrouter',
          model: 'anthropic/claude',
          input_tokens: 1200,
          output_tokens: 340,
          cost_source: 'provider_reported',
        })}
        tools={{}}
        onOpenTool={() => {}}
      />
    );
    const pill = container.querySelector('.cost-pill');
    expect(pill).toBeTruthy();
    expect(pill.textContent).toBe('$0.0031');
    expect(pill.getAttribute('title')).toContain('provider: openrouter');
    expect(pill.getAttribute('title')).toContain('model: anthropic/claude');
    expect(pill.getAttribute('title')).toContain('input: 1.2k tokens');
  });

  it('renders nothing for missing or audit-only cost metadata', () => {
    const missing = render(
      <Turn turn={assistantTurn(null)} tools={{}} onOpenTool={() => {}}/>
    );
    expect(missing.container.querySelector('.cost-pill')).toBeNull();
    cleanup();

    const pending = render(
      <Turn
        turn={assistantTurn({ swarm_llm_audit_id: 42 })}
        tools={{}}
        onOpenTool={() => {}}
      />
    );
    expect(pending.container.querySelector('.cost-pill')).toBeNull();
    expect(pending.container.textContent).not.toMatch(/cost unavailable|cost pending|-{2,}/i);
  });
});

describe('per-turn model label', () => {
  // Regression: the `.model` label was bound to `turn.model`, which is
  // never populated, so it was dead on hydrated transcripts. The model
  // actually rides on cost.model (the same field the cost-pill tooltip
  // reads), so a hydrated agent turn must surface it as a visible label.
  it('shows the model from cost.model on an assistant turn', () => {
    const { container } = render(
      <Turn
        turn={assistantTurn({ display_cost_usd: 0.002, model: 'deepseek/deepseek-v4-pro' })}
        tools={{}}
        onOpenTool={() => {}}
      />
    );
    const label = container.querySelector('.model');
    expect(label).toBeTruthy();
    expect(label.textContent).toBe('deepseek/deepseek-v4-pro');
  });

  it('renders no model label when cost has no model', () => {
    const { container } = render(
      <Turn turn={assistantTurn({ display_cost_usd: 0.002 })} tools={{}} onOpenTool={() => {}}/>
    );
    expect(container.querySelector('.model')).toBeNull();
  });

  it('renders no model label on a user turn', () => {
    const userTurn = { role: 'user', ts: '12:00:00', blocks: [{ type: 'text', text: 'hi' }] };
    const { container } = render(
      <Turn turn={userTurn} tools={{}} onOpenTool={() => {}}/>
    );
    expect(container.querySelector('.model')).toBeNull();
  });
});
