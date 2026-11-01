// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { renderHook } from '@testing-library/react';
import { AIProvider } from './aiContstants';
import { useProviderAvailability } from './useProviderAvailability';

const ollama = vi.hoisted(() =>
  vi.fn(() => ({ valid: false, checking: false })),
);
vi.mock('./useIsOllamaUrlValid', () => ({
  useIsOllamaUrlValid: ollama,
}));

afterEach(() => vi.clearAllMocks());

it('makes configured direct providers available without probing endpoints', () => {
  const { result } = renderHook(() =>
    useProviderAvailability({
      openRouterApiKey: undefined,
      ollamaUrl: undefined,
      openAIApiKey: 'openai-key',
      openAIModelId: 'gpt-custom',
      anthropicModelId: 'claude-custom',
      anthropicApiKey: 'anthropic-key',
      openAICompatibleBaseURL: '',
      openAICompatibleModelId: '',
    }),
  );

  expect(result.current.isProviderAvailable(AIProvider.OpenAI)).toBe(true);
  expect(result.current.isProviderAvailable(AIProvider.Anthropic)).toBe(true);
  expect(result.current.availableProviders).toEqual([
    AIProvider.OpenAI,
    AIProvider.Anthropic,
  ]);
  expect(ollama).toHaveBeenCalledWith(undefined);
});
it('does not mark a direct provider available with an empty model ID', () => {
  const { result } = renderHook(() =>
    useProviderAvailability({
      openRouterApiKey: undefined,
      ollamaUrl: undefined,
      openAIApiKey: 'openai-key',
      openAIModelId: '  ',
      anthropicApiKey: 'anthropic-key',
      anthropicModelId: '',
    }),
  );

  expect(result.current.isProviderAvailable(AIProvider.OpenAI)).toBe(false);
  expect(result.current.isProviderAvailable(AIProvider.Anthropic)).toBe(false);
});

it('requires a valid explicit compatible endpoint and model, but not a key', () => {
  const { result, rerender } = renderHook(
    ({ baseURL, modelId }: { baseURL: string; modelId: string }) =>
      useProviderAvailability({
        openRouterApiKey: undefined,
        ollamaUrl: undefined,
        openAICompatibleBaseURL: baseURL,
        openAICompatibleModelId: modelId,
      }),
    {
      initialProps: {
        baseURL: 'http://localhost:8317/v1',
        modelId: 'proxy-model',
      },
    },
  );

  expect(result.current.isProviderAvailable(AIProvider.OpenAICompatible)).toBe(
    true,
  );
  rerender({ baseURL: 'file:///tmp/api', modelId: 'proxy-model' });
  expect(result.current.isProviderAvailable(AIProvider.OpenAICompatible)).toBe(
    false,
  );
  rerender({ baseURL: 'https://proxy.example/v1', modelId: '' });
  expect(result.current.isProviderAvailable(AIProvider.OpenAICompatible)).toBe(
    false,
  );
  expect(ollama).toHaveBeenCalledTimes(3);
});
