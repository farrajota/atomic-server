import { createAnthropic } from '@ai-sdk/anthropic';
import { createOpenAI } from '@ai-sdk/openai';
import { AIProvider } from '@components/AI/aiContstants';
import type { AIModelIdentifier } from './types';

export interface ProviderModelSettings {
  openAIApiKey?: string;
  anthropicApiKey?: string;
  openAICompatibleApiKey?: string;
  openAICompatibleBaseURL?: string;
}

/** Returns a useful provider error message without configured API credentials. */
export function redactProviderError(
  error: unknown,
  credentials: readonly (string | undefined)[],
): string {
  const message =
    error instanceof Error
      ? error.message
      : typeof error === 'string'
        ? error
        : error && typeof error === 'object' && 'message' in error
          ? String(error.message)
          : 'The model request failed.';
  let safeMessage = message || 'The model request failed.';

  const secrets = credentials
    .flatMap(credential => {
      if (!credential) return [];
      const transmittedCredential = credential.trim();

      return transmittedCredential
        ? credential === transmittedCredential
          ? [credential]
          : [transmittedCredential, credential]
        : [];
    })
    .sort((a, b) => b.length - a.length);

  for (const secret of secrets) {
    safeMessage = safeMessage.replaceAll(secret, '[redacted]');
  }

  return safeMessage;
}

/** Normalize a compatible endpoint to the OpenAI API root without losing proxy paths. */
export function normalizeOpenAICompatibleBaseURL(value: string): string {
  let url: URL;

  try {
    url = new URL(value.trim());
  } catch {
    throw new Error('Enter a valid HTTP(S) OpenAI-compatible base URL.');
  }

  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new Error('The OpenAI-compatible base URL must use HTTP or HTTPS.');
  }

  if (url.username || url.password || url.search || url.hash) {
    throw new Error(
      'The OpenAI-compatible base URL cannot contain credentials, a query, or a fragment.',
    );
  }

  const path = url.pathname.replace(/\/+$/, '');
  url.pathname = /(?:^|\/)v1$/i.test(path) ? path || '/v1' : `${path}/v1`;

  return url.toString().replace(/\/$/, '');
}

/** Construct a native SDK model for each configured direct-provider protocol. */
export function createProviderModel(
  identifier: AIModelIdentifier,
  settings: ProviderModelSettings,
) {
  if (!identifier.id.trim())
    throw new Error(`Choose a model ID for ${identifier.provider}.`);

  switch (identifier.provider) {
    case AIProvider.OpenAI: {
      if (!settings.openAIApiKey?.trim())
        throw new Error(
          'Configure an OpenAI API key in AI settings to use this model.',
        );

      return createOpenAI({ apiKey: settings.openAIApiKey.trim() })(
        identifier.id,
      );
    }

    case AIProvider.Anthropic: {
      if (!settings.anthropicApiKey?.trim())
        throw new Error(
          'Configure an Anthropic API key in AI settings to use this model.',
        );

      return createAnthropic({
        apiKey: settings.anthropicApiKey.trim(),
        headers: { 'anthropic-dangerous-direct-browser-access': 'true' },
      })(identifier.id);
    }

    case AIProvider.OpenAICompatible: {
      if (!settings.openAICompatibleBaseURL?.trim())
        throw new Error(
          'Configure an OpenAI-compatible HTTP(S) base URL in AI settings to use this model.',
        );
      const apiKey = settings.openAICompatibleApiKey?.trim();
      const compatible = createOpenAI({
        baseURL: normalizeOpenAICompatibleBaseURL(
          settings.openAICompatibleBaseURL,
        ),
        apiKey: apiKey || 'keyless-endpoint',
        ...(!apiKey
          ? {
              fetch: async (input, init) => {
                const request = new Request(input, init);
                const headers = new Headers(request.headers);
                headers.delete('authorization');

                return globalThis.fetch(new Request(request, { headers }));
              },
            }
          : {}),
      });

      return compatible.chat(identifier.id);
    }

    default:
      throw new Error(
        `Provider ${identifier.provider} is not a direct provider.`,
      );
  }
}

/**
 * OpenAI's Responses SDK removes unsupported temperature values for its
 * reasoning models. Compatible chat-completion endpoints do not, so omit the
 * parameter for recognizable OpenAI reasoning model IDs there.
 */
export function getDirectProviderParameters(
  identifier: AIModelIdentifier,
  temperature: number | undefined,
): { temperature?: number } {
  if (identifier.provider === AIProvider.OpenAI) return { temperature };

  if (
    identifier.provider === AIProvider.OpenAICompatible &&
    !isOpenAIReasoningModel(identifier.id)
  ) {
    return { temperature };
  }

  return {};
}

function isOpenAIReasoningModel(modelId: string): boolean {
  const modelFamilyId = modelId.slice(modelId.lastIndexOf('/') + 1);
  if (/^o\d+(?:-|$)/i.test(modelFamilyId)) return true;

  const match = /^gpt-(\d+)(?:\.(\d+))?(?:-(.+))?$/i.exec(modelFamilyId);
  if (!match) return false;

  const isChatVariant = match[3]?.toLowerCase().startsWith('chat') ?? false;

  return Number(match[1]) >= 5 && !isChatVariant;
}
