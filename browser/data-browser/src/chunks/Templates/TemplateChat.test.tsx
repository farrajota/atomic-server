// @vitest-environment jsdom
// @wc-ignore-file
import React from 'react';
import { cleanup, fireEvent, render, waitFor } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { ThemeProvider } from 'styled-components';
import { AIProvider } from '@components/AI/aiContstants';
import { buildTheme } from '../../styling';
import TemplateChat from './TemplateChat';

const state = vi.hoisted(() => ({
  settings: {
    openRouterApiKey: '',
    openAIApiKey: '',
    anthropicApiKey: '',
    openAICompatibleApiKey: '',
    openAICompatibleBaseURL: 'https://proxy.example/templates/v1',
    openRouterZdr: false,
    genFeaturesModel: { provider: 'openai-compatible', id: 'template-model' },
    defaultChatModel: { provider: 'hosted', id: 'included-model' },
    isProviderAvailable: () => true,
  },
  report: vi.fn(),
}));

vi.mock('@components/AI/AISettingsContext', () => ({
  useAISettings: () => state.settings,
}));
vi.mock('../RTE/AIChatInput/AsyncAIChatInput', () => ({
  default: ({
    onChange,
    onSubmit,
    disabled,
  }: {
    onChange: (value: string) => void;
    onSubmit: () => void;
    disabled: boolean;
  }) => (
    <>
      <textarea
        aria-label='Workspace request'
        onChange={event => onChange(event.target.value)}
        disabled={disabled}
      />
      <button onClick={onSubmit} disabled={disabled}>
        Send
      </button>
    </>
  ),
}));
vi.mock('../../components/SideBar/FeedbackButton', () => ({
  FeedbackButton: ({ reportError }: { reportError: Error }) => (
    <button onClick={() => state.report(reportError)}>Report failure</button>
  ),
}));

afterEach(() => {
  cleanup();
  state.report.mockReset();
  vi.unstubAllGlobals();
});

it.each([
  {
    provider: AIProvider.OpenAI,
    path: '/responses',
    header: 'authorization',
    key: 'template-openai-key',
  },
  {
    provider: AIProvider.Anthropic,
    path: '/messages',
    header: 'x-api-key',
    key: 'template-anthropic-key',
  },
  {
    provider: AIProvider.OpenAICompatible,
    path: '/chat/completions',
    header: 'authorization',
    key: 'template-compatible-key',
  },
])(
  'sanitizes $provider request failures before ErrorBlock display and reporting after settings change',
  async ({ provider, path, header, key }) => {
    Object.assign(state.settings, {
      openRouterApiKey: '  template-openrouter-key  ',
      openAIApiKey: '  template-openai-key  ',
      anthropicApiKey: '  template-anthropic-key  ',
      openAICompatibleApiKey: '  template-compatible-key  ',
      genFeaturesModel: { provider, id: 'template-model' },
    });
    const credentials = [
      state.settings.openRouterApiKey,
      state.settings.openAIApiKey,
      state.settings.anthropicApiKey,
      state.settings.openAICompatibleApiKey,
    ];
    const echoedKeys = credentials
      .map(value => `configured=${value}; transmitted=${value.trim()}`)
      .join('; ');
    const theme = buildTheme(false, '#1b50d8');
    const onProposal = vi.fn();
    const fetch = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const request = new Request(input, init);
        expect(new URL(request.url).pathname.endsWith(path)).toBe(true);
        expect(request.headers.get(header)).toBe(
          header === 'authorization' ? `Bearer ${key}` : key,
        );
        const body = JSON.parse(await request.text());
        expect(body.model).toBe('template-model');

        // A rerender must not replace the credentials attached to the pending request.
        Object.assign(state.settings, {
          openRouterApiKey: 'rotated-openrouter-key',
          openAIApiKey: 'rotated-openai-key',
          anthropicApiKey: 'rotated-anthropic-key',
          openAICompatibleApiKey: 'rotated-compatible-key',
        });
        view.rerender(
          <ThemeProvider theme={theme}>
            <TemplateChat onProposal={onProposal} />
          </ThemeProvider>,
        );

        return new Response(
          JSON.stringify({
            type: 'error',
            error: {
              type: 'authentication_error',
              message: `Template authentication rejected: ${echoedKeys}; request=template-401`,
            },
          }),
          {
            status: 401,
            headers: { 'content-type': 'application/json' },
          },
        );
      },
    );
    vi.stubGlobal('fetch', fetch);
    const view = render(
      <ThemeProvider theme={theme}>
        <TemplateChat onProposal={onProposal} />
      </ThemeProvider>,
    );
    fireEvent.change(
      await view.findByRole('textbox', { name: 'Workspace request' }),
      {
        target: { value: 'Plan a workspace' },
      },
    );
    fireEvent.click(view.getByRole('button', { name: 'Send' }));

    const report = await view.findByRole('button', { name: 'Report failure' });
    const safeMessage = `Template authentication rejected: ${credentials.map(() => 'configured=[redacted]; transmitted=[redacted]').join('; ')}; request=template-401`;
    expect(view.container.textContent).toContain(safeMessage);

    for (const credential of credentials) {
      expect(view.container.textContent).not.toContain(credential.trim());
    }

    fireEvent.click(report);
    expect(state.report).toHaveBeenCalledTimes(1);
    const reported = state.report.mock.calls[0][0] as Error;
    expect(reported).toBeInstanceOf(Error);
    expect(reported.message).toBe(safeMessage);
    expect(reported.cause).toBeUndefined();

    for (const credential of credentials) {
      expect(`${reported.message}\n${reported.stack}`).not.toContain(
        credential.trim(),
      );
    }

    expect(fetch).toHaveBeenCalledTimes(1);
    expect(onProposal).not.toHaveBeenCalled();
    await waitFor(() => {
      const sendButton = view.getByRole('button', {
        name: 'Send',
      }) as HTMLButtonElement;
      expect(sendButton.disabled).toBe(false);
    });
  },
);
