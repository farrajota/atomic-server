import { describe, expect, it } from 'vitest';
import { AIProvider } from '@components/AI/aiContstants';
import {
  getDirectProviderModelOptions,
  parseChatModelSelection,
} from './RealAIChat';

describe('RealAIChat direct provider selection', () => {
  it('offers configured providers and preserves their model identity', () => {
    const options = getDirectProviderModelOptions({
      isProviderAvailable: () => true,
      openAIModelId: 'gpt-4.1-mini',
      anthropicModelId: 'claude-sonnet-4-5',
      openAICompatibleModelId: 'tenant/custom:model',
    });

    expect(options.map(option => option.value)).toEqual([
      'openai:gpt-4.1-mini',
      'anthropic:claude-sonnet-4-5',
      'openai-compatible:tenant/custom:model',
    ]);
    expect(
      options.map(option => parseChatModelSelection(option.value)),
    ).toEqual([
      { provider: AIProvider.OpenAI, id: 'gpt-4.1-mini' },
      { provider: AIProvider.Anthropic, id: 'claude-sonnet-4-5' },
      { provider: AIProvider.OpenAICompatible, id: 'tenant/custom:model' },
    ]);
  });

  it('omits direct providers that are unavailable', () => {
    const options = getDirectProviderModelOptions({
      isProviderAvailable: provider => provider === AIProvider.Anthropic,
      openAIModelId: 'gpt-4.1-mini',
      anthropicModelId: 'claude-sonnet-4-5',
      openAICompatibleModelId: 'custom-model',
    });

    expect(options.map(option => option.value)).toEqual([
      'anthropic:claude-sonnet-4-5',
    ]);
  });

  it('does not reinterpret an unknown provider as Ollama', () => {
    expect(parseChatModelSelection('future-provider:model')).toBeUndefined();
  });
});
