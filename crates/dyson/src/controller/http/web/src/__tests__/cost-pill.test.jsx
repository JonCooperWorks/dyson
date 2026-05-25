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
