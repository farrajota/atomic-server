import { afterEach, describe, expect, it, vi } from 'vitest';
import type { UIMessageChunk } from 'ai';
import { AIProvider } from '@components/AI/aiContstants';
import {
  ClientOnlyTransport,
  type ClientOnlyTransportOptions,
} from './ClientOnlyTransport';

const directProviders = [
  { provider: AIProvider.OpenAI, path: '/responses', header: 'authorization' },
  { provider: AIProvider.Anthropic, path: '/messages', header: 'x-api-key' },
  {
    provider: AIProvider.OpenAICompatible,
    path: '/chat/completions',
    header: 'authorization',
  },
];

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('ClientOnlyTransport error boundaries', () => {
  it.each(directProviders)(
    'reports $provider stream failures safely while preserving UI errors and request credentials',
    async ({ provider, path, header }) => {
      const configuredKey = '  transport-fixture-key  ';
      const options: ClientOnlyTransportOptions = {
        model: { provider, id: 'boundary-model' },
        openAIApiKey: configuredKey,
        anthropicApiKey: configuredKey,
        openAICompatibleApiKey: configuredKey,
        openAICompatibleBaseURL: 'https://proxy.example/custom/v1',
        selectedAgent: {
          id: 'boundary-agent',
          name: 'Boundary agent',
          description: '',
          systemPrompt: 'Respond to the user.',
          availableTools: [],
          canReadAtomicData: false,
          canWriteAtomicData: false,
          ragEnabled: false,
          skillsEnabled: false,
          temperature: 0.4,
        },
        tools: {},
        addContextToMessages: async messages => messages,
        resolveOutputModalities: () => ['text'],
        resolveParameterSupport: () => true,
      };
      const transport = new ClientOnlyTransport(
        options,
        () => 'assistant-message',
        async prompt => prompt,
      );
      const logged = vi.spyOn(console, 'error').mockImplementation(() => {});
      const fetch = vi.fn(
        async (input: RequestInfo | URL, init?: RequestInit) => {
          const request = new Request(input, init);
          expect(new URL(request.url).pathname.endsWith(path)).toBe(true);
          expect(request.headers.get(header)).toBe(
            header === 'authorization'
              ? 'Bearer transport-fixture-key'
              : 'transport-fixture-key',
          );
          const body = JSON.parse(await request.text());
          expect(body.model).toBe('boundary-model');
          expect(body.stream).toBe(true);
          expect(body.temperature).toBe(0.4);

          // The failure belongs to the old request even if settings change in flight.
          transport.setOptions({
            ...options,
            openAIApiKey: 'rotated-fixture-key',
            anthropicApiKey: 'rotated-fixture-key',
            openAICompatibleApiKey: 'rotated-fixture-key',
          });

          return new Response(
            JSON.stringify({
              type: 'error',
              error: {
                type: 'authentication_error',
                message: `Authentication rejected: configured=${configuredKey}; transmitted=transport-fixture-key; request=fixture-401`,
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

      const stream = await transport.sendMessages({
        chatId: 'boundary-chat',
        trigger: 'submit-message',
        messageId: 'user-message',
        messages: [
          {
            id: 'user-message',
            role: 'user',
            parts: [{ type: 'text', text: 'Hello' }],
          },
        ],
        abortSignal: new AbortController().signal,
      });
      const chunks: UIMessageChunk[] = [];
      const reader = stream.getReader();

      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(value);
      }

      const safeMessage =
        'Authentication rejected: configured=[redacted]; transmitted=[redacted]; request=fixture-401';
      expect(fetch).toHaveBeenCalledTimes(1);
      expect(chunks).toContainEqual({ type: 'error', errorText: safeMessage });
      expect(logged).toHaveBeenCalledTimes(1);
      expect(logged).toHaveBeenCalledWith(
        'AI chat streaming failed',
        safeMessage,
      );
      expect(JSON.stringify(logged.mock.calls)).not.toContain(
        'transport-fixture-key',
      );
      expect(JSON.stringify(chunks)).not.toContain('transport-fixture-key');
    },
  );
});
