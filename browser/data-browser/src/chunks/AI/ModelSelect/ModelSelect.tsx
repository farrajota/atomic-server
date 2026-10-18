import { useState } from 'react';
import { Column } from '@components/Row';
import { BasicSelect } from '@components/forms/BasicSelect';
import { AIProvider } from '@components/AI/aiContstants';
import { type AIModelIdentifier } from '../types';
import { OpenRouterModelSelector } from './OpenRouterModelSelector';
import { OllamaModelSelector } from './OllamaModelSelector';
import { Link } from '@tanstack/react-router';
import { useAISettings } from '@components/AI/AISettingsContext';
import { InputStyled } from '@components/forms/InputStyles';
import { Button } from '@components/Button';

interface ModelSelectProps {
  onSelect?: (model: AIModelIdentifier) => void;
  defaultModel: AIModelIdentifier;
  enforceToolSupport?: boolean;
}

const PROVIDERS = [
  { label: 'OpenRouter', value: AIProvider.OpenRouter },
  { label: 'Ollama', value: AIProvider.Ollama },
  { label: 'OpenAI', value: AIProvider.OpenAI },
  { label: 'Anthropic', value: AIProvider.Anthropic },
  { label: 'OpenAI-compatible', value: AIProvider.OpenAICompatible },
];

export const ModelSelect = ({
  onSelect,
  defaultModel,
  enforceToolSupport = false,
}: ModelSelectProps) => {
  const {
    openRouterApiKey,
    ollamaUrl,
    hostedAI,
    openAIModelId,
    anthropicModelId,
    openAICompatibleModelId,
    isProviderAvailable,
  } = useAISettings();

  const [provider, setProvider] = useState(defaultModel.provider);
  const providers = [
    ...(hostedAI?.enabled
      ? [{ label: 'Included AI', value: AIProvider.Hosted }]
      : []),
    ...PROVIDERS.filter(
      option =>
        option.value === AIProvider.OpenRouter ||
        option.value === AIProvider.Ollama ||
        isProviderAvailable(option.value) ||
        option.value === defaultModel.provider,
    ),
  ];
  const configuredModelId =
    provider === AIProvider.OpenAI
      ? openAIModelId
      : provider === AIProvider.Anthropic
        ? anthropicModelId
        : provider === AIProvider.OpenAICompatible
          ? openAICompatibleModelId
          : '';
  const modelId =
    defaultModel.provider === provider ? defaultModel.id : configuredModelId;

  return (
    <Column gap='0.75rem'>
      <BasicSelect
        aria-label='Provider'
        value={provider}
        onChange={event => setProvider(event.target.value as AIProvider)}
      >
        {providers.map(option => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </BasicSelect>
      {provider === AIProvider.Hosted && hostedAI?.enabled && (
        <Column gap='0.5rem'>
          <p>{`${Math.floor(hostedAI.remaining_micros / 1000)} credits remaining across your drives.`}</p>
          <Button
            onClick={() =>
              onSelect?.({
                id: hostedAI.model,
                provider: AIProvider.Hosted,
              })
            }
          >
            Use included model
          </Button>
        </Column>
      )}
      {provider === AIProvider.OpenRouter && (
        <>
          {openRouterApiKey ? (
            <OpenRouterModelSelector
              enforceToolSupport={enforceToolSupport}
              onSelect={model => {
                onSelect?.(model);
              }}
              defaultModel={
                defaultModel.provider === AIProvider.OpenRouter
                  ? defaultModel.id
                  : ''
              }
            />
          ) : (
            <Column>
              <span>
                OpenRouter API key is not configured. Go to{' '}
                <Link to='/app/settings'>Settings</Link>.
              </span>
            </Column>
          )}
        </>
      )}
      {provider === AIProvider.Ollama && (
        <>
          {ollamaUrl ? (
            <OllamaModelSelector
              onSelect={model => {
                onSelect?.(model);
              }}
              selectedModel={
                defaultModel.provider === AIProvider.Ollama
                  ? defaultModel
                  : { provider: AIProvider.Ollama, id: '' }
              }
            />
          ) : (
            <Column>
              <span>
                Ollama URL is not configured. Go to{' '}
                <Link to='/app/settings'>Settings</Link>.
              </span>
            </Column>
          )}
        </>
      )}
      {(provider === AIProvider.OpenAI ||
        provider === AIProvider.Anthropic ||
        provider === AIProvider.OpenAICompatible) && (
        <Column gap='0.5rem'>
          {!isProviderAvailable(provider) && (
            <span>
              {provider === AIProvider.OpenAICompatible
                ? 'Configure a valid HTTP(S) endpoint and model ID in AI Settings. An API key is optional.'
                : 'Configure this provider API key and model ID in AI Settings.'}
            </span>
          )}
          <label htmlFor='configured-provider-model'>Model ID</label>
          <InputStyled
            id='configured-provider-model'
            value={modelId}
            onChange={event => onSelect?.({ id: event.target.value, provider })}
          />
        </Column>
      )}
    </Column>
  );
};

export default ModelSelect;
