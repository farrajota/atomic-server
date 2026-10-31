import { createOllama } from 'ollama-ai-provider-v2';
import type { AIModelIdentifier } from './types';
import { createOpenRouter } from '@openrouter/ai-sdk-provider';
import { AIProvider } from '@components/AI/aiContstants';
import { useAISettings } from '@components/AI/AISettingsContext';
import type { LanguageModel } from 'ai';
import { createHostedModel } from './hostedModel';
import { createProviderModel } from './providerModel';

const createOpenRouterProvider = (openRouterApiKey: string, zdr: boolean) => {
  return createOpenRouter({
    apiKey: openRouterApiKey,
    compatibility: 'strict',
    extraBody: {
      transforms: ['middle-out'],
      ...(zdr ? { provider: { zdr: true } } : {}),
    },
  });
};

const createOllamaProvider = (ollamaUrl: string) => {
  return createOllama({
    baseURL: `${ollamaUrl}/api`,
  });
};

export function useGetModel(): (
  identifier: AIModelIdentifier,
) => LanguageModel | undefined {
  const {
    openRouterApiKey,
    ollamaUrl,
    isProviderAvailable,
    openRouterZdr,
    openAIApiKey,
    anthropicApiKey,
    openAICompatibleApiKey,
    openAICompatibleBaseURL,
  } = useAISettings();

  return (identifier: AIModelIdentifier): LanguageModel | undefined => {
    const directProvider =
      identifier.provider === AIProvider.OpenAI ||
      identifier.provider === AIProvider.Anthropic ||
      identifier.provider === AIProvider.OpenAICompatible;

    if (!directProvider && !isProviderAvailable(identifier.provider)) {
      return undefined;
    }

    if (directProvider) {
      return createProviderModel(identifier, {
        openAIApiKey,
        anthropicApiKey,
        openAICompatibleApiKey,
        openAICompatibleBaseURL,
      });
    }

    if (identifier.provider === AIProvider.Hosted) {
      return createHostedModel(identifier.id);
    }

    if (identifier.provider === AIProvider.OpenRouter) {
      if (!openRouterApiKey) {
        return undefined;
      }

      return createOpenRouterProvider(
        openRouterApiKey,
        openRouterZdr,
      )(identifier.id);
    }

    if (identifier.provider === AIProvider.Ollama) {
      if (!ollamaUrl) {
        return undefined;
      }

      return createOllamaProvider(ollamaUrl)(identifier.id);
    }
  };
}
