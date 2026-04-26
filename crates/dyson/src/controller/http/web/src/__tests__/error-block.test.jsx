// Renders a turn that contains an `error` block and asserts the
// distinct error card is in the DOM, with the message text and the
// red-tinted visual class.  Replaces the prior "[error] …" prose
// block — making sure the new path didn't regress to a missing
// renderer (which would render nothing) or a fall-through to the
// generic text path.

import React from 'react';
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { render, cleanup } from '@testing-library/react';
import { Turn, ErrorBlock } from '../components/turns.jsx';

beforeEach(() => {});
afterEach(() => { cleanup(); });

describe('ErrorBlock', () => {
  it('renders the message inside an .errorblock card with role=alert', () => {
    const block = {
      type: 'error',
      message: 'OpenRouter (402 Payment Required): Prompt tokens limit exceeded — add credits',
    };
    const { container, getByText } = render(<ErrorBlock block={block}/>);
    const card = container.querySelector('.errorblock');
    expect(card, 'error block must render an .errorblock card').toBeTruthy();
    expect(card.getAttribute('role')).toBe('alert');
    expect(getByText(block.message)).toBeTruthy();
    // The "error" tag is uppercased via CSS — assert the underlying
    // text is the lowercase token so a future copy change is caught
    // without depending on text-transform.
    expect(container.querySelector('.errorblock-tag').textContent).toBe('error');
  });

  it('Turn renders error blocks via the b.type === "error" branch', () => {
    const turn = {
      role: 'agent',
      ts: '12:00',
      blocks: [
        { type: 'text', text: 'Working on it…' },
        { type: 'error', message: 'Anthropic (529): Overloaded' },
      ],
    };
    const { container } = render(
      <Turn turn={turn} turnIndex={0} tools={{}} activeTool={null}
            onOpenTool={() => {}} onRate={() => {}} ratable={false}
            rating={null}/>
    );
    expect(container.querySelector('.errorblock')).toBeTruthy();
    expect(container.textContent).toContain('Anthropic (529): Overloaded');
    // The preceding text block must still render — the error block
    // doesn't replace surrounding content.
    expect(container.textContent).toContain('Working on it');
  });
});
