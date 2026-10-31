// @vitest-environment jsdom
// @wc-ignore-file
import React from 'react';
import { cleanup, fireEvent, render } from '@testing-library/react';
import { ThemeProvider, type DefaultTheme } from 'styled-components';
import { afterEach, expect, it, vi } from 'vitest';
import { AIProvider } from '@components/AI/aiContstants';
import type { AIModelIdentifier } from '../types';
import { ModelSelect } from './ModelSelect';

const settings = vi.hoisted(() => ({
  openRouterApiKey: undefined as string | undefined,
  ollamaUrl: 'http://ollama.invalid' as string | undefined,
  hostedAI: undefined,
  openAIModelId: 'gpt-4.1-configured',
  anthropicModelId: 'claude-sonnet-4-configured',
  openAICompatibleModelId: 'gpt-6.1-sol',
  isProviderAvailable: () => true,
}));

vi.mock('@components/AI/AISettingsContext', () => ({
  useAISettings: () => settings,
}));

// These legacy selector UIs are outside the provider-notification contract.
vi.mock('./OpenRouterModelSelector', () => ({
  OpenRouterModelSelector: () => null,
}));
vi.mock('./OllamaModelSelector', () => ({
  OllamaModelSelector: () => null,
}));

afterEach(() => {
  cleanup();
});

it.each([
  [AIProvider.OpenAI, 'gpt-4.1-configured'],
  [AIProvider.Anthropic, 'claude-sonnet-4-configured'],
  [AIProvider.OpenAICompatible, 'gpt-6.1-sol'],
] as const)(
  'notifies the selected direct provider with its configured model ID (%s)',
  (provider, modelId) => {
    const onSelect = vi.fn<(model: AIModelIdentifier) => void>();
    const view = render(
      <ThemeProvider theme={{ colors: {} } as DefaultTheme}>
        <ModelSelect
          defaultModel={{
            provider: AIProvider.Ollama,
            id: 'stale-ollama-model',
          }}
          onSelect={onSelect}
        />
      </ThemeProvider>,
    );

    fireEvent.change(view.getByRole('combobox', { name: 'Provider' }), {
      target: { value: provider },
    });

    expect(onSelect).toHaveBeenCalledTimes(1);
    expect(onSelect).toHaveBeenCalledWith({ provider, id: modelId });
  },
);

it('uses the saved same-provider model override after browsing away and back', () => {
  const onSelect = vi.fn<(model: AIModelIdentifier) => void>();
  const view = render(
    <ThemeProvider theme={{ colors: {} } as DefaultTheme}>
      <ModelSelect
        defaultModel={{
          provider: AIProvider.OpenAI,
          id: 'saved-openai-override',
        }}
        onSelect={onSelect}
      />
    </ThemeProvider>,
  );
  const providerSelect = view.getByRole('combobox', { name: 'Provider' });

  fireEvent.change(providerSelect, { target: { value: AIProvider.Anthropic } });
  fireEvent.change(providerSelect, { target: { value: AIProvider.OpenAI } });

  expect(onSelect).toHaveBeenNthCalledWith(1, {
    provider: AIProvider.Anthropic,
    id: 'claude-sonnet-4-configured',
  });
  expect(onSelect).toHaveBeenNthCalledWith(2, {
    provider: AIProvider.OpenAI,
    id: 'saved-openai-override',
  });
  expect(onSelect).toHaveBeenCalledTimes(2);
});

it('leaves legacy provider selection to its existing model-choice flow', () => {
  const onSelect = vi.fn<(model: AIModelIdentifier) => void>();
  const view = render(
    <ThemeProvider theme={{ colors: {} } as DefaultTheme}>
      <ModelSelect
        defaultModel={{
          provider: AIProvider.OpenAI,
          id: 'saved-openai-override',
        }}
        onSelect={onSelect}
      />
    </ThemeProvider>,
  );

  fireEvent.change(view.getByRole('combobox', { name: 'Provider' }), {
    target: { value: AIProvider.Ollama },
  });

  expect(onSelect).not.toHaveBeenCalled();
});
