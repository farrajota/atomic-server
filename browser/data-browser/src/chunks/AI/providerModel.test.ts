import { afterEach, describe, expect, it, vi } from 'vitest';
import { generateText, stepCountIs, streamText, tool } from 'ai';
import { z } from 'zod';
import { AIProvider } from '@components/AI/aiContstants';
import {
  createProviderModel,
  getDirectProviderParameters,
  normalizeOpenAICompatibleBaseURL,
  redactProviderError,
} from './providerModel';

const response = (body: unknown) =>
  new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });

afterEach(() => vi.unstubAllGlobals());

describe('direct provider request boundaries', () => {
  it('uses distinct native endpoints, model IDs, and provider authentication', async () => {
    const requests: Array<{ url: string; headers: Headers; body: string }> = [];
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const request = new Request(input, init);
        requests.push({
          url: request.url,
          headers: request.headers,
          body: await request.text(),
        });

        if (request.url.includes('/messages')) {
          return response({
            id: 'msg_test',
            type: 'message',
            role: 'assistant',
            model: 'claude-test',
            content: [{ type: 'text', text: 'ok' }],
            stop_reason: 'end_turn',
            stop_sequence: null,
            usage: { input_tokens: 1, output_tokens: 1 },
          });
        }

        if (request.url.includes('/responses')) {
          return response({
            id: 'resp_test',
            object: 'response',
            status: 'completed',
            created_at: 1_700_000_000,
            model: 'gpt-test',
            output: [
              {
                id: 'msg_test',
                type: 'message',
                role: 'assistant',
                status: 'completed',
                content: [{ type: 'output_text', text: 'ok', annotations: [] }],
              },
            ],
            usage: { input_tokens: 1, output_tokens: 1 },
          });
        }

        return response({
          id: 'chatcmpl_test',
          object: 'chat.completion',
          model: 'proxy-test',
          choices: [
            {
              index: 0,
              message: { role: 'assistant', content: 'ok' },
              finish_reason: 'stop',
            },
          ],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        });
      }),
    );

    await generateText({
      model: createProviderModel(
        { provider: AIProvider.OpenAI, id: 'gpt-test' },
        { openAIApiKey: 'openai-secret' },
      ),
      prompt: 'test',
      temperature: 0.6,
    });
    await generateText({
      model: createProviderModel(
        { provider: AIProvider.Anthropic, id: 'claude-test' },
        { anthropicApiKey: 'anthropic-secret' },
      ),
      prompt: 'test',
    });
    await generateText({
      model: createProviderModel(
        { provider: AIProvider.OpenAICompatible, id: 'proxy-test' },
        {
          openAICompatibleBaseURL: 'https://proxy.example/custom/path/v1/',
          openAICompatibleApiKey: 'proxy-secret',
        },
      ),
      prompt: 'test',
      temperature: 0.7,
    });

    expect(requests.map(({ url }) => url)).toEqual([
      'https://api.openai.com/v1/responses',
      'https://api.anthropic.com/v1/messages',
      'https://proxy.example/custom/path/v1/chat/completions',
    ]);
    expect(requests.map(({ body }) => JSON.parse(body).model)).toEqual([
      'gpt-test',
      'claude-test',
      'proxy-test',
    ]);
    expect(requests.map(({ body }) => JSON.parse(body).temperature)).toEqual([
      0.6,
      undefined,
      0.7,
    ]);
    expect(requests[0].headers.get('authorization')).toBe(
      'Bearer openai-secret',
    );
    expect(requests[1].headers.get('x-api-key')).toBe('anthropic-secret');
    expect(
      requests[1].headers.get('anthropic-dangerous-direct-browser-access'),
    ).toBe('true');
    expect(requests[2].headers.get('authorization')).toBe(
      'Bearer proxy-secret',
    );
  });

  it('supports keyless compatible endpoints and preserves custom path prefixes', async () => {
    expect(
      normalizeOpenAICompatibleBaseURL('https://proxy.example/gateway'),
    ).toBe('https://proxy.example/gateway/v1');
    expect(
      normalizeOpenAICompatibleBaseURL('https://proxy.example/gateway/v1/'),
    ).toBe('https://proxy.example/gateway/v1');
    let authorization: string | null = 'unset';
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        authorization = new Request(input, init).headers.get('authorization');

        return response({
          id: 'chatcmpl_test',
          object: 'chat.completion',
          model: 'local-model',
          choices: [
            {
              index: 0,
              message: { role: 'assistant', content: 'ok' },
              finish_reason: 'stop',
            },
          ],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        });
      }),
    );
    await generateText({
      model: createProviderModel(
        { provider: AIProvider.OpenAICompatible, id: 'local-model' },
        { openAICompatibleBaseURL: 'http://localhost:8080/api/v1' },
      ),
      prompt: 'test',
    });
    expect(authorization).toBeNull();
  });

  it('streams compatible chat completions from the configured endpoint', async () => {
    const chunks = [
      'data: {"id":"chunk-1","object":"chat.completion.chunk","model":"stream-model","choices":[{"index":0,"delta":{"content":"streamed"},"finish_reason":null}]}\n\n',
      'data: {"id":"chunk-2","object":"chat.completion.chunk","model":"stream-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n',
      'data: [DONE]\n\n',
    ].join('');
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(chunks, {
            status: 200,
            headers: { 'content-type': 'text/event-stream' },
          }),
      ),
    );
    const result = streamText({
      model: createProviderModel(
        { provider: AIProvider.OpenAICompatible, id: 'stream-model' },
        { openAICompatibleBaseURL: 'https://proxy.example/v1' },
      ),
      prompt: 'test',
    });
    let text = '';
    for await (const part of result.textStream) text += part;
    expect(text).toBe('streamed');
  });

  it('executes tools through compatible chat completions', async () => {
    let calls = 0;
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => {
        calls += 1;

        return response(
          calls === 1
            ? {
                id: 'chatcmpl_tool',
                object: 'chat.completion',
                model: 'tool-model',
                choices: [
                  {
                    index: 0,
                    message: {
                      role: 'assistant',
                      content: null,
                      tool_calls: [
                        {
                          id: 'call_1',
                          type: 'function',
                          function: {
                            name: 'answer',
                            arguments: '{"value":"done"}',
                          },
                        },
                      ],
                    },
                    finish_reason: 'tool_calls',
                  },
                ],
              }
            : {
                id: 'chatcmpl_answer',
                object: 'chat.completion',
                model: 'tool-model',
                choices: [
                  {
                    index: 0,
                    message: { role: 'assistant', content: 'completed' },
                    finish_reason: 'stop',
                  },
                ],
              },
        );
      }),
    );
    const result = await generateText({
      model: createProviderModel(
        { provider: AIProvider.OpenAICompatible, id: 'tool-model' },
        { openAICompatibleBaseURL: 'https://proxy.example/v1' },
      ),
      prompt: 'use the tool',
      tools: {
        answer: tool({
          inputSchema: z.object({ value: z.string() }),
          execute: async ({ value }) => value,
        }),
      },
      stopWhen: stepCountIs(2),
    });
    expect(result.text).toBe('completed');
    expect(calls).toBe(2);
  });

  it('passes supported direct temperatures and omits reasoning temperature for compatible models', () => {
    expect(
      getDirectProviderParameters(
        { provider: AIProvider.OpenAI, id: 'gpt-4.1-mini' },
        0.4,
      ),
    ).toEqual({ temperature: 0.4 });
    expect(
      getDirectProviderParameters(
        { provider: AIProvider.OpenAICompatible, id: 'custom-model' },
        0.7,
      ),
    ).toEqual({ temperature: 0.7 });
    expect(
      getDirectProviderParameters(
        { provider: AIProvider.OpenAICompatible, id: 'o3' },
        0.7,
      ),
    ).toEqual({});
    expect(
      getDirectProviderParameters(
        { provider: AIProvider.OpenAICompatible, id: 'tenant/o3-mini' },
        0.7,
      ),
    ).toEqual({});
    expect(
      getDirectProviderParameters(
        { provider: AIProvider.OpenAICompatible, id: 'gpt-5-chat-latest' },
        0.7,
      ),
    ).toEqual({ temperature: 0.7 });
  });

  it('redacts the trimmed credential actually sent by a provider', () => {
    expect(
      redactProviderError(
        new Error('Provider echoed transmitted-secret in its response.'),
        ['  transmitted-secret  '],
      ),
    ).toBe('Provider echoed [redacted] in its response.');
  });

  it('lets the OpenAI Responses SDK omit temperature for reasoning models', async () => {
    let requestBody: Record<string, unknown> | undefined;
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const request = new Request(input, init);
        requestBody = JSON.parse(await request.text());

        return response({
          id: 'resp_reasoning',
          object: 'response',
          status: 'completed',
          created_at: 1_700_000_000,
          model: 'o3',
          output: [
            {
              id: 'msg_reasoning',
              type: 'message',
              role: 'assistant',
              status: 'completed',
              content: [{ type: 'output_text', text: 'ok', annotations: [] }],
            },
          ],
          usage: { input_tokens: 1, output_tokens: 1 },
        });
      }),
    );

    const identifier = { provider: AIProvider.OpenAI, id: 'o3' } as const;
    await generateText({
      model: createProviderModel(identifier, { openAIApiKey: 'openai-secret' }),
      prompt: 'test',
      ...getDirectProviderParameters(identifier, 0.4),
    });

    expect(requestBody?.temperature).toBeUndefined();
  });
  it('reports missing configuration without exposing credentials', () => {
    expect(() =>
      createProviderModel({ provider: AIProvider.OpenAI, id: 'gpt-test' }, {}),
    ).toThrow('Configure an OpenAI API key in AI settings');
    expect(() =>
      createProviderModel(
        { provider: AIProvider.OpenAICompatible, id: 'proxy-test' },
        { openAICompatibleApiKey: 'must-not-leak' },
      ),
    ).toThrow('Configure an OpenAI-compatible HTTP(S) base URL');
    expect(() =>
      createProviderModel(
        { provider: AIProvider.Anthropic, id: 'claude-test' },
        {},
      ),
    ).toThrow('Configure an Anthropic API key');
  });
});
