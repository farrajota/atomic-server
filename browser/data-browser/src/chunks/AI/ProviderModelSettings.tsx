import { useAISettings } from '@components/AI/AISettingsContext';
import { AIProvider } from '@components/AI/aiContstants';
import { OpenRouterModelSelector } from './ModelSelect/OpenRouterModelSelector';
import { OllamaModelSelector } from './ModelSelect/OllamaModelSelector';
import { Button } from '@components/Button';
import { Column } from '@components/Row';

export default function ProviderModelSettings({
  provider,
}: {
  provider: AIProvider;
}) {
  const {
    defaultChatModel,
    setDefaultChatModel,
    isProviderAvailable,
    openAIModelId,
    anthropicModelId,
    openAICompatibleModelId,
  } = useAISettings();
  if (!isProviderAvailable(provider)) return null;
  const selected = defaultChatModel.provider === provider;

  return (
    <Column gap='0.5rem'>
      <span>
        {selected
          ? 'Default chat model'
          : 'Choose a model to use this provider for new chats'}
      </span>
      {provider === AIProvider.OpenAI && (
        <Button
          onClick={() => setDefaultChatModel({ id: openAIModelId, provider })}
        >
          Use OpenAI for new chats
        </Button>
      )}
      {provider === AIProvider.Anthropic && (
        <Button
          onClick={() =>
            setDefaultChatModel({ id: anthropicModelId, provider })
          }
        >
          Use Anthropic for new chats
        </Button>
      )}
      {provider === AIProvider.OpenAICompatible && (
        <Button
          onClick={() =>
            setDefaultChatModel({ id: openAICompatibleModelId, provider })
          }
        >
          Use compatible model for new chats
        </Button>
      )}
      {provider === AIProvider.OpenRouter && (
        <OpenRouterModelSelector
          key={selected ? defaultChatModel.id : 'unselected'}
          defaultModel={selected ? defaultChatModel.id : ''}
          onSelect={setDefaultChatModel}
          enforceToolSupport
        />
      )}
      {provider === AIProvider.Ollama && (
        <OllamaModelSelector
          selectedModel={selected ? defaultChatModel : { provider, id: '' }}
          onSelect={setDefaultChatModel}
        />
      )}
    </Column>
  );
}
