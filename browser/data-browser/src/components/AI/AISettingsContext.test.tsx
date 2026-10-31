// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import React from 'react';
import { act, renderHook } from '@testing-library/react';
import type { PropsWithChildren } from 'react';
import { AISettingsContextProvider, useAISettings } from './AISettingsContext';
import { AIProvider } from './aiContstants';
const hosted = vi.hoisted(() => ({ enabled: false }));

vi.mock('./useHostedAI', () => ({
  useHostedAI: () => ({
    hostedAI: hosted.enabled
      ? { enabled: true, model: 'included-model' }
      : undefined,
    setHostedAI: vi.fn(),
  }),
}));
vi.mock('./useIsOllamaUrlValid', () => ({
  useIsOllamaUrlValid: () => ({ valid: false, checking: false }),
}));

afterEach(() => {
  window.localStorage.clear();
  hosted.enabled = false;
});

it('persists provider keys, endpoint, and editable model IDs in browser storage', () => {
  const wrapper = ({ children }: PropsWithChildren) => (
    <AISettingsContextProvider>{children}</AISettingsContextProvider>
  );
  const { result } = renderHook(() => useAISettings(), { wrapper });

  expect(result.current.openAIModelId).toBe('gpt-4.1-mini');
  expect(result.current.anthropicModelId).toBe('claude-sonnet-4-5');
  expect(result.current.openAICompatibleModelId).toBe('');

  act(() => {
    result.current.setOpenAIApiKey('openai-test-key');
    result.current.setAnthropicApiKey('anthropic-test-key');
    result.current.setOpenAICompatibleApiKey('proxy-test-key');
    result.current.setOpenAICompatibleBaseURL('https://proxy.example/v1');
    result.current.setOpenAIModelId('gpt-custom');
    result.current.setAnthropicModelId('claude-custom');
    result.current.setOpenAICompatibleModelId('proxy-custom');
  });

  expect(window.localStorage.getItem('atomic.ai.openai-api-key')).toBe(
    '"openai-test-key"',
  );
  expect(window.localStorage.getItem('atomic.ai.anthropic-api-key')).toBe(
    '"anthropic-test-key"',
  );
  expect(
    window.localStorage.getItem('atomic.ai.openai-compatible-api-key'),
  ).toBe('"proxy-test-key"');
  expect(
    window.localStorage.getItem('atomic.ai.openai-compatible-base-url'),
  ).toBe('"https://proxy.example/v1"');
  expect(result.current.openAIModelId).toBe('gpt-custom');
  expect(result.current.anthropicModelId).toBe('claude-custom');
  expect(result.current.openAICompatibleModelId).toBe('proxy-custom');
  expect(result.current.isProviderAvailable(AIProvider.OpenAI)).toBe(true);
  expect(result.current.isProviderAvailable(AIProvider.Anthropic)).toBe(true);
  expect(result.current.isProviderAvailable(AIProvider.OpenAICompatible)).toBe(
    true,
  );
});
it('does not replace a selected personal provider with included AI', () => {
  hosted.enabled = true;
  const selectedModel = { id: 'gpt-custom', provider: AIProvider.OpenAI };
  window.localStorage.setItem(
    'atomic.ai.defaultChatModel',
    JSON.stringify(selectedModel),
  );
  window.localStorage.setItem(
    'atomic.ai.genFeaturesModel',
    JSON.stringify(selectedModel),
  );
  const wrapper = ({ children }: PropsWithChildren) => (
    <AISettingsContextProvider>{children}</AISettingsContextProvider>
  );
  const { result } = renderHook(() => useAISettings(), { wrapper });

  expect(result.current.defaultChatModel).toEqual(selectedModel);
  expect(result.current.genFeaturesModel).toEqual(selectedModel);
});
