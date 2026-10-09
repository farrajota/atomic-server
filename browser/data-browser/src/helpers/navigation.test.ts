// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { constructOpenURL } from './navigation';

describe('constructOpenURL', () => {
  afterEach(() => {
    window.history.replaceState({}, '', '/');
  });

  it('keeps extra route parameters when opening a same-origin resource', () => {
    const origin = window.location.origin;

    expect(
      constructOpenURL(`${origin}/table`, { view: `${origin}/view` }),
    ).toBe(`/table?view=${encodeURIComponent(`${origin}/view`)}`);
  });

  it('returns the original path and query when there are no extra parameters', () => {
    const origin = window.location.origin;

    expect(constructOpenURL(`${origin}/table?filter=active&sort=name`)).toBe(
      '/table?filter=active&sort=name',
    );
  });

  it('lets an extra parameter override an existing one of the same name', () => {
    const origin = window.location.origin;

    expect(
      constructOpenURL(`${origin}/table?view=old&filter=active`, {
        view: 'atomic:view:new',
      }),
    ).toBe('/table?view=atomic%3Aview%3Anew&filter=active');
  });

  it('preserves existing same-origin query parameters alongside extra parameters', () => {
    const origin = window.location.origin;

    expect(
      constructOpenURL(`${origin}/table?filter=active`, {
        drive: 'atomic:drive:reader-scope',
        view: 'atomic:view:review',
      }),
    ).toBe(
      '/table?filter=active&drive=atomic%3Adrive%3Areader-scope&view=atomic%3Aview%3Areview',
    );
  });
});
