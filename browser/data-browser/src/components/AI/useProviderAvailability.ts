import { AIProvider } from './aiContstants';
import { useIsOllamaUrlValid } from './useIsOllamaUrlValid';

type ProviderAvailabilitySettings = {
  openRouterApiKey: string | undefined;
  ollamaUrl: string | undefined;
  hostedAvailable?: boolean;
  openAIApiKey?: string | undefined;
  anthropicApiKey?: string | undefined;
  openAIModelId?: string;
  anthropicModelId?: string;
  openAICompatibleBaseURL?: string | undefined;
  openAICompatibleModelId?: string;
};

const isHttpUrl = (value: string | undefined): boolean => {
  if (!value) return false;

  try {
    const url = new URL(value);

    return (
      (url.protocol === 'http:' || url.protocol === 'https:') &&
      Boolean(url.hostname)
    );
  } catch {
    return false;
  }
};

export const useProviderAvailability = ({
  openRouterApiKey,
  ollamaUrl,
  hostedAvailable = false,
  openAIApiKey,
  anthropicApiKey,
  openAIModelId,
  anthropicModelId,
  openAICompatibleBaseURL,
  openAICompatibleModelId,
}: ProviderAvailabilitySettings) => {
  const openRouterAvailable = Boolean(openRouterApiKey);
  const { valid: ollamaAvailable, checking: ollamaChecking } =
    useIsOllamaUrlValid(ollamaUrl);
  const openAIAvailable = Boolean(
    openAIApiKey?.trim() && openAIModelId?.trim(),
  );
  const anthropicAvailable = Boolean(
    anthropicApiKey?.trim() && anthropicModelId?.trim(),
  );
  const openAICompatibleAvailable =
    isHttpUrl(openAICompatibleBaseURL) &&
    Boolean(openAICompatibleModelId?.trim());

  const isProviderAvailable = (provider: AIProvider) => {
    if (provider === AIProvider.Hosted) return hostedAvailable;
    if (provider === AIProvider.OpenRouter) return openRouterAvailable;
    if (provider === AIProvider.Ollama) return ollamaAvailable;
    if (provider === AIProvider.OpenAI) return openAIAvailable;
    if (provider === AIProvider.Anthropic) return anthropicAvailable;
    if (provider === AIProvider.OpenAICompatible)
      return openAICompatibleAvailable;

    return false;
  };

  const availableProviders: AIProvider[] = [];
  if (hostedAvailable) availableProviders.push(AIProvider.Hosted);
  if (openRouterAvailable) availableProviders.push(AIProvider.OpenRouter);
  if (ollamaAvailable) availableProviders.push(AIProvider.Ollama);
  if (openAIAvailable) availableProviders.push(AIProvider.OpenAI);
  if (anthropicAvailable) availableProviders.push(AIProvider.Anthropic);
  if (openAICompatibleAvailable)
    availableProviders.push(AIProvider.OpenAICompatible);

  return {
    openRouterAvailable,
    ollamaAvailable,
    openAIAvailable,
    anthropicAvailable,
    openAICompatibleAvailable,
    ollamaChecking,
    isProviderAvailable,
    availableProviders,
  };
};
