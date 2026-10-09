import { afterEach, beforeAll, describe, it, vi } from 'vitest';
import { enableLoro } from './loro-loader.js';
import { Resource, ResourceEvents } from './resource.js';
import { testStore } from './test-store.js';

const legacyProperty = 'did:ad:property:custom-kb-field';
const canonicalProperty = 'atomic:property:custom-kb-field';
const resourceSubject = 'atomic:resource:property-alias-test';

/**
 * One resource whose history holds the property under both spellings, as
 * when a legacy and a current client each wrote it: their Loro histories
 * merge. A single client's `set` edits the spelling already stored instead.
 */
async function storedUnderBothSpellings(
  legacyValue: string | string[],
  canonicalValue: string | string[],
): Promise<Resource> {
  const legacyWriter = new Resource(resourceSubject);
  await legacyWriter.set(legacyProperty, legacyValue, false);
  legacyWriter.sealPendingEdits();
  const canonicalWriter = new Resource(resourceSubject);
  await canonicalWriter.set(canonicalProperty, canonicalValue, false);
  canonicalWriter.sealPendingEdits();

  const merged = new Resource(resourceSubject);
  merged.importLoroUpdate(
    legacyWriter.getLoroDoc()!.export({ mode: 'snapshot' }),
  );
  merged.importLoroUpdate(
    canonicalWriter.getLoroDoc()!.export({ mode: 'snapshot' }),
  );

  return merged;
}

beforeAll(async () => {
  await enableLoro();
});

// A failing assertion skips a test's own `mockRestore`; restoring here keeps
// one test's console.warn spy calls out of the next test's assertions.
afterEach(() => {
  vi.restoreAllMocks();
});

describe('Resource custom property access across identifier spellings', () => {
  it('reads both property spellings from signed history without constraining projection keys', async ({
    expect,
  }) => {
    const { store, posted, agentDID } = await testStore();
    const fieldValue = 'retained custom field';
    const original = await store.newResource({
      subject: resourceSubject,
      did: false,
    });
    await original.set(legacyProperty, fieldValue, false);
    await original.save();

    const signedCommit = posted.find(
      commit => commit.subject === resourceSubject,
    );
    expect(signedCommit).toBeDefined();
    expect(signedCommit?.signature).toBeTruthy();
    expect(signedCommit?.signer).toBe(agentDID);
    expect(signedCommit?.loroUpdate).toBeInstanceOf(Uint8Array);

    const signature = signedCommit!.signature;
    const signedUpdate = signedCommit!.loroUpdate!.slice();
    const reloaded = new Resource(resourceSubject);
    reloaded.importLoroUpdate(signedUpdate);
    const historyBeforeReads = reloaded
      .getLoroDoc()!
      .export({ mode: 'snapshot' });
    const creator = original.getCreatedBy();
    const createdAt = original.getCreatedAt();

    expect(creator).toBe(agentDID);
    expect(createdAt).toBeDefined();
    expect(reloaded.get(canonicalProperty)).toBe(fieldValue);
    expect(reloaded.get(legacyProperty)).toBe(fieldValue);
    expect(reloaded.getCreatedBy()).toBe(creator);
    expect(reloaded.getCreatedAt()).toBe(createdAt);
    expect(reloaded.getLoroDoc()!.export({ mode: 'snapshot' })).toEqual(
      historyBeforeReads,
    );
    expect(signedCommit!.signature).toBe(signature);
    expect(signedCommit!.loroUpdate).toEqual(signedUpdate);
  });

  it('does not hide canonical property values from legacy schema references', async ({
    expect,
  }) => {
    const resource = new Resource(resourceSubject);
    await resource.set(canonicalProperty, 'canonical value', false);

    expect(resource.get(legacyProperty)).toBe('canonical value');
  });

  it('edits a value in place under the spelling it is stored with', async ({
    expect,
  }) => {
    const resource = new Resource(resourceSubject);
    await resource.set(legacyProperty, 'old value', false);
    const changed: string[] = [];
    resource.on(ResourceEvents.LocalChange, prop => changed.push(prop));

    await resource.set(canonicalProperty, 'new value', false);

    expect(resource.get(canonicalProperty)).toBe('new value');
    expect(resource.get(legacyProperty)).toBe('new value');
    expect(Object.keys(resource.getPropVals())).toContain(legacyProperty);
    expect(Object.keys(resource.getPropVals())).not.toContain(
      canonicalProperty,
    );
    expect(
      resource.getPropertyAliasConflict(canonicalProperty),
    ).toBeUndefined();
    expect(changed).toEqual([canonicalProperty, legacyProperty]);
  });

  it('removes a value stored under the other spelling', async ({ expect }) => {
    const resource = new Resource(resourceSubject);
    await resource.set(canonicalProperty, 'stored canonically', false);

    resource.remove(legacyProperty);

    expect(resource.get(legacyProperty)).toBeUndefined();
    expect(resource.get(canonicalProperty)).toBeUndefined();
    expect(Object.keys(resource.getPropVals())).not.toContain(
      canonicalProperty,
    );
  });

  it('removes unsafely a value stored under the other spelling', async ({
    expect,
  }) => {
    const resource = new Resource(resourceSubject);
    await resource.set(legacyProperty, 'stored legacy', false);

    resource.removeUnsafe(canonicalProperty);

    expect(resource.get(canonicalProperty)).toBeUndefined();
    expect(Object.keys(resource.getPropVals())).not.toContain(legacyProperty);
  });

  it('removes both spellings when both hold different values', async ({
    expect,
  }) => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const resource = await storedUnderBothSpellings(
      'legacy value',
      'canonical value',
    );
    const changed: string[] = [];
    resource.on(ResourceEvents.LocalChange, prop => changed.push(prop));

    resource.remove(canonicalProperty);

    expect(resource.get(legacyProperty)).toBeUndefined();
    expect(resource.get(canonicalProperty)).toBeUndefined();
    expect(Object.keys(resource.getPropVals())).not.toContain(legacyProperty);
    expect(Object.keys(resource.getPropVals())).not.toContain(
      canonicalProperty,
    );
    expect(changed).toEqual([canonicalProperty, legacyProperty]);

    // The removal is part of the history, so the other spelling's value does
    // not come back for a reader that loads it.
    const reloaded = new Resource(resourceSubject);
    reloaded.importLoroUpdate(
      resource.getLoroDoc()!.export({ mode: 'snapshot' }),
    );
    expect(reloaded.get(legacyProperty)).toBeUndefined();
    expect(reloaded.get(canonicalProperty)).toBeUndefined();
    warn.mockRestore();
  });

  it('removes unsafely both spellings when both hold different values', async ({
    expect,
  }) => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const resource = await storedUnderBothSpellings(
      'legacy value',
      'canonical value',
    );

    resource.removeUnsafe(legacyProperty);

    expect(resource.get(legacyProperty)).toBeUndefined();
    expect(resource.get(canonicalProperty)).toBeUndefined();
    expect(Object.keys(resource.getPropVals())).not.toContain(
      canonicalProperty,
    );
    warn.mockRestore();
  });

  it('sets both spellings when both hold different values, settling the conflict', async ({
    expect,
  }) => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const resource = await storedUnderBothSpellings(
      'legacy value',
      'canonical value',
    );
    const changed: string[] = [];
    resource.on(ResourceEvents.LocalChange, prop => changed.push(prop));

    await resource.set(canonicalProperty, 'chosen value', false);

    expect(resource.get(legacyProperty)).toBe('chosen value');
    expect(resource.get(canonicalProperty)).toBe('chosen value');
    expect(resource.getPropVals()).toMatchObject({
      [legacyProperty]: 'chosen value',
      [canonicalProperty]: 'chosen value',
    });
    expect(
      resource.getPropertyAliasConflict(canonicalProperty),
    ).toBeUndefined();
    expect(changed).toEqual([canonicalProperty, legacyProperty]);
    warn.mockRestore();
  });

  it('reads one value when both spellings store the same value', async ({
    expect,
  }) => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const resource = await storedUnderBothSpellings(
      ['same', 'value'],
      ['same', 'value'],
    );

    expect(resource.get(legacyProperty)).toEqual(['same', 'value']);
    expect(resource.get(canonicalProperty)).toEqual(['same', 'value']);
    expect(resource.getPropertyAliasConflict(legacyProperty)).toBeUndefined();
    expect(warn).not.toHaveBeenCalled();
    warn.mockRestore();
  });

  it('diagnoses conflicting spellings and keeps both stored values', async ({
    expect,
  }) => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const resource = await storedUnderBothSpellings(
      'legacy value',
      'canonical value',
    );

    expect(resource.get(legacyProperty)).toBe('legacy value');
    expect(resource.get(canonicalProperty)).toBe('canonical value');
    expect(resource.getPropertyAliasConflict(canonicalProperty)).toEqual({
      property: canonicalProperty,
      value: 'canonical value',
      alias: legacyProperty,
      aliasValue: 'legacy value',
    });
    expect(resource.getPropVals()).toMatchObject({
      [legacyProperty]: 'legacy value',
      [canonicalProperty]: 'canonical value',
    });

    // Rendering reads the same property on every update; one warning per
    // logical property is the diagnosis, not one per read.
    resource.get(legacyProperty);
    expect(warn).toHaveBeenCalledTimes(1);
    expect(String(warn.mock.calls[0][0])).toContain(
      'stored under both scheme spellings with different values',
    );
    expect(String(warn.mock.calls[0][0])).not.toContain('legacy value');
    expect(String(warn.mock.calls[0][0])).not.toContain('canonical value');
    warn.mockRestore();
  });
});
