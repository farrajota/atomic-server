// @vitest-environment jsdom
// @wc-ignore-file
import React from 'react';
import { cleanup, fireEvent, render, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { ThemeProvider, type DefaultTheme } from 'styled-components';
import { AISetupPanel } from './AISetupPanel';
import type { HostedAIStatus } from '@helpers/managed/ai';

const providerSettings = vi.hoisted(() => ({
  compatibleAvailable: false,
  openAIApiKey: 'stored-openai-key',
  anthropicApiKey: 'stored-anthropic-key',
  openAICompatibleApiKey: 'stored-compatible-key',
  openAICompatibleBaseURL: 'https://models.example.test/v1',
  openAIModelId: 'gpt-4.1',
  anthropicModelId: 'claude-sonnet-4',
  openAICompatibleModelId: 'gpt-6.1-sol',
}));

const hosted = vi.hoisted(() => ({
  status: undefined as HostedAIStatus | undefined,
  enable: vi.fn(),
  managed: false,
}));

beforeEach(() => {
  hosted.status = undefined;
  hosted.managed = false;
  hosted.enable.mockReset();
  providerSettings.compatibleAvailable = false;
});

vi.mock('@components/Row', () => ({ Column: 'div', Row: 'div' }));
vi.mock('@helpers/managed/api', () => ({
  hasManagedApi: () => hosted.managed,
}));
vi.mock('@chunks/AI/ModelSelect/ModelSelect', () => ({ default: () => null }));

vi.mock('@components/Dialog', () => ({
  useDialog: ({ onCancel }: { onCancel: () => void }) => [
    { onCancel },
    () => {},
  ],
  Dialog: ({
    children,
    onCancel,
  }: React.PropsWithChildren<{ onCancel: () => void }>) => (
    <div role='dialog'>
      <button onClick={onCancel}>Dismiss</button>
      {children}
    </div>
  ),
  DialogTitle: ({ children }: React.PropsWithChildren) => <div>{children}</div>,
  DialogContent: ({ children }: React.PropsWithChildren) => (
    <div>{children}</div>
  ),
  DialogActions: ({ children }: React.PropsWithChildren) => (
    <div>{children}</div>
  ),
}));
vi.mock('@components/AI/AISettingsContext', () => ({
  DEFAULT_CHAT_MODEL: { id: 'test', provider: 'openrouter' },
  useAISettings: () => ({
    hostedAI: hosted.status,
    enableIncludedAI: hosted.enable,
    availableProviders: providerSettings.compatibleAvailable
      ? ['openai-compatible']
      : [],
    isProviderAvailable: (provider: string) =>
      provider === 'openai-compatible' && providerSettings.compatibleAvailable,
    openRouterAvailable: false,
    ollamaAvailable: false,
    openAIApiKey: providerSettings.openAIApiKey,
    setOpenAIApiKey: vi.fn(),
    anthropicApiKey: providerSettings.anthropicApiKey,
    setAnthropicApiKey: vi.fn(),
    openAICompatibleApiKey: providerSettings.openAICompatibleApiKey,
    setOpenAICompatibleApiKey: vi.fn(),
    openAICompatibleBaseURL: providerSettings.openAICompatibleBaseURL,
    setOpenAICompatibleBaseURL: vi.fn(),
    openAIModelId: providerSettings.openAIModelId,
    setOpenAIModelId: vi.fn(),
    anthropicModelId: providerSettings.anthropicModelId,
    setAnthropicModelId: vi.fn(),
    openAICompatibleModelId: providerSettings.openAICompatibleModelId,
    setOpenAICompatibleModelId: vi.fn(),
    setDefaultChatModel: vi.fn(),
    defaultChatModel: {
      id: providerSettings.openAICompatibleModelId,
      provider: 'openai-compatible',
    },
  }),
}));
vi.mock('./AgentConfig', () => ({
  useAIAgentConfig: () => ({ agents: [], saveAgents: vi.fn() }),
}));
vi.mock('@components/AI/useIsOllamaUrlValid', () => ({
  useIsOllamaUrlValid: () => ({ checking: false }),
}));
vi.mock('@components/AI/LocalOllamaDiscovery', () => ({
  LocalOllamaDiscovery: () => null,
}));
vi.mock('@components/AI/ProviderStatus', () => ({
  ProviderStatus: () => null,
}));
vi.mock('@components/AI/OpenRouterLoginButton', () => ({
  OpenRouterLoginButton: () => null,
}));
vi.mock('@components/Button', () => ({
  Button: ({
    subtle: _subtle,
    ...props
  }: React.ButtonHTMLAttributes<HTMLButtonElement> & { subtle?: boolean }) => {
    void _subtle;

    return <button {...props} />;
  },
}));
vi.mock('@components/OutlinedSection', () => ({
  OutlinedSection: ({
    children,
    title,
  }: React.PropsWithChildren<{ title: string }>) => (
    <section>
      <h2>{title}</h2>
      {children}
    </section>
  ),
}));
vi.mock('@components/forms/InputStyles', () => ({
  InputStyled: 'input',
  InputWrapper: 'div',
}));

afterEach(() => {
  cleanup();
  localStorage.clear();
  sessionStorage.clear();
});

it('reopens a dismissed setup when chat requests it again', () => {
  const panel = (requestId: number) => (
    <ThemeProvider theme={{ colors: {} } as DefaultTheme}>
      <AISetupPanel requestId={requestId} />
    </ThemeProvider>
  );
  const view = render(panel(0));
  fireEvent.click(view.getByText('Dismiss'));
  expect(view.queryByRole('dialog')).toBeNull();
  view.rerender(panel(1));
  expect(view.queryByRole('dialog')).not.toBeNull();
  fireEvent.click(view.getByText('Dismiss'));
  view.rerender(panel(2));
  expect(view.queryByRole('dialog')).not.toBeNull();
});

it('shows configured provider choices and allows continuing with compatible AI', () => {
  providerSettings.compatibleAvailable = true;
  const view = render(
    <ThemeProvider theme={{ colors: {} } as DefaultTheme}>
      <AISetupPanel />
    </ThemeProvider>,
  );

  for (const provider of [
    'OpenRouter',
    'Ollama',
    'OpenAI',
    'Anthropic',
    'OpenAI-compatible',
  ]) {
    expect(view.getByRole('heading', { name: provider })).toBeTruthy();
  }

  const openAIKey = view.getByLabelText(
    /^OpenAI API key$/i,
  ) as HTMLInputElement;
  const openAIModel = view.getByLabelText(
    /^OpenAI model ID$/i,
  ) as HTMLInputElement;
  const anthropicKey = view.getByLabelText(
    /^Anthropic API key$/i,
  ) as HTMLInputElement;
  const anthropicModel = view.getByLabelText(
    /^Anthropic model ID$/i,
  ) as HTMLInputElement;
  const compatibleKey = view.getByLabelText(
    /^OpenAI-compatible API key \(optional\)$/i,
  ) as HTMLInputElement;
  const baseURL = view.getByLabelText(
    /^OpenAI-compatible base URL$/i,
  ) as HTMLInputElement;
  const compatibleModel = view.getByLabelText(
    /^OpenAI-compatible model ID$/i,
  ) as HTMLInputElement;

  expect(openAIKey.type).toBe('password');
  expect(openAIKey.value).toBe(providerSettings.openAIApiKey);
  expect(openAIModel.value).toBe(providerSettings.openAIModelId);
  expect(anthropicKey.type).toBe('password');
  expect(anthropicKey.value).toBe(providerSettings.anthropicApiKey);
  expect(anthropicModel.value).toBe(providerSettings.anthropicModelId);
  expect(compatibleKey.type).toBe('password');
  expect(compatibleKey.value).toBe(providerSettings.openAICompatibleApiKey);
  expect(compatibleKey.required).toBe(false);
  expect(openAIKey.disabled).toBe(false);
  expect(openAIModel.disabled).toBe(false);
  expect(anthropicKey.disabled).toBe(false);
  expect(anthropicModel.disabled).toBe(false);
  expect(compatibleKey.disabled).toBe(false);
  expect(baseURL.type).toBe('url');
  expect(baseURL.value).toBe(providerSettings.openAICompatibleBaseURL);
  expect(baseURL.disabled).toBe(false);
  expect(compatibleModel.value).toBe('gpt-6.1-sol');
  expect(compatibleModel.disabled).toBe(false);
  fireEvent.click(view.getByRole('button', { name: 'Continue' }));
  const startChatting = view.getByRole('button', {
    name: 'Start chatting',
  }) as HTMLButtonElement;
  expect(startChatting.disabled).toBe(false);
  fireEvent.click(startChatting);
  expect(view.queryByRole('dialog')).toBeNull();
});

it('requires an explicit included-AI choice and keeps failed setup retryable', async () => {
  hosted.status = {
    enabled: true,
    consent: false,
    paid: false,
    model: 'google/gemini-2.5-flash',
    allowance_micros: 100000,
    used_micros: 0,
    remaining_micros: 100000,
    resets_at: 1790812800,
  };
  hosted.enable.mockRejectedValueOnce(
    new Error('Could not enable included AI.'),
  );
  const view = render(
    <ThemeProvider theme={{ colors: {} } as DefaultTheme}>
      <AISetupPanel />
    </ThemeProvider>,
  );
  expect(hosted.enable).not.toHaveBeenCalled();
  fireEvent.click(view.getByRole('button', { name: 'Use included AI' }));
  await waitFor(() =>
    expect(view.getByRole('alert').textContent).toContain('Could not enable'),
  );
  expect(view.getByRole('dialog')).not.toBeNull();
  hosted.enable.mockResolvedValueOnce(undefined);
  fireEvent.click(view.getByRole('button', { name: 'Use included AI' }));
  await waitFor(() => expect(view.queryByRole('dialog')).toBeNull());
  expect(hosted.enable).toHaveBeenCalledTimes(2);
});

it('never opens onboarding for a SaaS instance, even before status has loaded', () => {
  hosted.managed = true;
  const panel = (requestId: number) => (
    <ThemeProvider theme={{ colors: {} } as DefaultTheme}>
      <AISetupPanel requestId={requestId} />
    </ThemeProvider>
  );
  const view = render(panel(0));
  expect(view.queryByRole('dialog')).toBeNull();
  expect(hosted.enable).not.toHaveBeenCalled();
  view.rerender(panel(1));
  expect(view.getByRole('dialog')).not.toBeNull();
});
