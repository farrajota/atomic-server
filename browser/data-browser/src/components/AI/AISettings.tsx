import { Panel, usePanelList } from '@components/SideBar/usePanelList';
import SpeechSettings from './SpeechSettings';
import { AIProvider } from './aiContstants';
import { LocalOllamaDiscovery } from '@components/AI/LocalOllamaDiscovery';
import * as React from 'react';
import { Column, Row } from '@components/Row';
import { Checkbox, CheckboxLabel } from '@components/forms/Checkbox';
import { InputStyled, InputWrapper } from '@components/forms/InputStyles';
import styled, { useTheme } from 'styled-components';
import { Suspense, useEffect, useState } from 'react';
import { OpenRouterLoginButton } from './OpenRouterLoginButton';
import { effectFetch } from '@helpers/effectFetch';
import { transition } from '@helpers/transition';
import { useAISettings } from './AISettingsContext';
import { useIsOllamaUrlValid } from './useIsOllamaUrlValid';
import { Details } from '@components/Details';
import {
  SettingsContent,
  SettingsSection,
  SettingsSectionWrapper,
  SettingsLabel,
  useSettingsSearch,
  SettingsSearchProvider,
  queryMatches,
} from '@components/Settings';
import { WarningBlock } from '@components/WarningBlock';
import { FaCheck, FaTriangleExclamation } from 'react-icons/fa6';

const ProviderModelSettings = React.lazy(
  () => import('@chunks/AI/ProviderModelSettings'),
);

const AIConfigurationSections = React.lazy(
  () => import('@chunks/AI/AIConfigurationSections'),
);

const ModelSelect = React.lazy(
  () => import('@chunks/AI/ModelSelect/ModelSelect'),
);

const intl = new Intl.NumberFormat('default', {
  style: 'currency',
  currency: 'USD',
  currencyDisplay: 'narrowSymbol',
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

interface CreditUsage {
  total: number;
  used: number;
}

const CREDITS_ENDPOINT = 'https://openrouter.ai/api/v1/credits';

// Keywords for the AI section's own content (enable toggle, token usage)
const AI_OWN_KEYWORDS = 'ai enable features token usage panel sidebar';
// Keywords from child sections — makes this section visible, but children still filter
const AI_CHILD_KEYWORDS =
  'openrouter ollama openai anthropic openai-compatible endpoint cors browser-local storage mcp server generative model chat provider api key local agents skills default voice microphone speech stt transcription privacy retention zdr generate titles follow up prompts temperature context tools system prompt content references transport headers';

const AISettings: React.FC = () => {
  const theme = useTheme();
  const { enabledPanels, enablePanel, disablePanel } = usePanelList();
  const { query: searchQuery } = useSettingsSearch();
  const {
    enableAI,
    setEnableAI,
    openRouterApiKey,
    setOpenRouterApiKey,
    showTokenUsage,
    setShowTokenUsage,
    ollamaUrl,
    setOllamaUrl,
    isProviderAvailable,
    shouldGenerateTitles,
    setShouldGenerateTitles,
    genFeaturesModel,
    openAIApiKey,
    setOpenAIApiKey,
    anthropicApiKey,
    setAnthropicApiKey,
    openAICompatibleApiKey,
    setOpenAICompatibleApiKey,
    openAICompatibleBaseURL,
    setOpenAICompatibleBaseURL,
    openAIModelId,
    setOpenAIModelId,
    anthropicModelId,
    setAnthropicModelId,
    openAICompatibleModelId,
    defaultChatModel,
    setDefaultChatModel,
    setOpenAICompatibleModelId,
    setGenFeaturesModel,
  } = useAISettings();

  const [creditUsage, setCreditUsage] = useState<CreditUsage | undefined>();

  const handleSetOpenRouterKey = (key: string | undefined) => {
    if (!key) {
      setCreditUsage(undefined);
    }

    setOpenRouterApiKey(key);
  };

  const genFeaturesUnavailable = !isProviderAvailable(
    genFeaturesModel.provider,
  );

  const { valid: isOllamaUrlValid } = useIsOllamaUrlValid(ollamaUrl);

  useEffect(() => {
    if (!openRouterApiKey) {
      return;
    }

    return effectFetch(CREDITS_ENDPOINT, {
      headers: {
        Authorization: `Bearer ${openRouterApiKey}`,
      },
    })(data => {
      setCreditUsage({
        total: data.data.total_credits,
        used: data.data.total_usage,
      });
    });
  }, [openRouterApiKey]);

  const { parentMatched } = useSettingsSearch();
  const isSearching = searchQuery.length > 0;

  const ownMatch =
    isSearching && queryMatches(searchQuery, `ai ${AI_OWN_KEYWORDS}`);
  const childMatch =
    isSearching &&
    !ownMatch &&
    queryMatches(searchQuery, `ai ${AI_CHILD_KEYWORDS}`);

  const wholeSectionMatch = searchQuery.trim().toLowerCase() === 'ai';
  const matches = (keywords: string) =>
    !isSearching ||
    parentMatched ||
    wholeSectionMatch ||
    queryMatches(searchQuery, `ai ${keywords}`);

  // Only propagate parentMatched when this section's own content matched,
  // not when a child keyword matched (let children filter themselves).
  const childContext = React.useMemo(
    () => ({
      query: searchQuery,
      parentMatched: parentMatched || wholeSectionMatch,
    }),
    [searchQuery, parentMatched, wholeSectionMatch],
  );

  if (isSearching && !ownMatch && !childMatch && !parentMatched) {
    return null;
  }

  return (
    <SettingsSectionWrapper>
      <Details
        noIndent
        title={<SettingsLabel>AI</SettingsLabel>}
        open={isSearching}
        initialState={isSearching}
      >
        <SettingsContent>
          <SettingsSearchProvider value={childContext}>
            <Column gap='0.75rem'>
              {matches('enable ai features') && (
                <CheckboxLabel>
                  <Checkbox checked={enableAI} onChange={setEnableAI} />
                  <span>Enable AI Features</span>
                </CheckboxLabel>
              )}
              {matches('enable ai chats panel sidebar') && (
                <CheckboxLabel>
                  <Checkbox
                    checked={enabledPanels.has(Panel.AIChats)}
                    onChange={checked =>
                      checked
                        ? enablePanel(Panel.AIChats)
                        : disablePanel(Panel.AIChats)
                    }
                  />
                  <span>Enable AI Chats panel</span>
                </CheckboxLabel>
              )}
              <ConditionalSettings enabled={enableAI} inert={!enableAI}>
                <SubGroup>
                  {matches(
                    'generative features generate titles model show token usage chats',
                  ) && (
                    <SettingsSection
                      label='Generative features'
                      childSearchKeywords='generative features generate chat titles model show token usage chats'
                    >
                      {genFeaturesUnavailable && (
                        <WarningBlock>
                          <WarningBlock.Title>
                            The generative features model uses a provider that
                            is not available.
                          </WarningBlock.Title>
                        </WarningBlock>
                      )}
                      <Column gap='0.5rem'>
                        {matches(
                          'generative features show token usage chats',
                        ) && (
                          <CheckboxLabel>
                            <Checkbox
                              checked={showTokenUsage}
                              onChange={setShowTokenUsage}
                            />
                            <span>Show token usage in chats</span>
                          </CheckboxLabel>
                        )}
                        {matches(
                          'generative features generate chat titles',
                        ) && (
                          <CheckboxLabel>
                            <Checkbox
                              checked={shouldGenerateTitles}
                              onChange={setShouldGenerateTitles}
                            />
                            <span>Generate AI Chat titles</span>
                          </CheckboxLabel>
                        )}

                        {matches('generative features model') && (
                          <Column gap='0.5rem'>
                            <span>Provider</span>
                            <Suspense>
                              <ModelSelect
                                defaultModel={genFeaturesModel}
                                onSelect={setGenFeaturesModel}
                              />
                            </Suspense>
                          </Column>
                        )}
                      </Column>
                    </SettingsSection>
                  )}
                  {matches('openai api key model default') && (
                    <SettingsSection
                      label='OpenAI'
                      childSearchKeywords='openai api key model default'
                    >
                      <Column gap='0.5rem'>
                        <label htmlFor='openai-api-key'>OpenAI API key</label>
                        <InputWrapper>
                          <InputStyled
                            id='openai-api-key'
                            type='password'
                            autoComplete='new-password'
                            value={openAIApiKey || ''}
                            onChange={event =>
                              setOpenAIApiKey(event.target.value || undefined)
                            }
                          />
                        </InputWrapper>
                        <Subtle>
                          This key stays in this browser's local storage.
                          Requests are sent directly over HTTPS from this
                          browser to OpenAI and require provider access and
                          browser CORS permission.
                        </Subtle>
                        <label htmlFor='openai-model-id'>OpenAI model ID</label>
                        <InputWrapper>
                          <InputStyled
                            id='openai-model-id'
                            value={openAIModelId}
                            onChange={event => {
                              const id = event.target.value;
                              setOpenAIModelId(id);

                              if (
                                defaultChatModel.provider === AIProvider.OpenAI
                              ) {
                                setDefaultChatModel({
                                  id,
                                  provider: AIProvider.OpenAI,
                                });
                              }
                            }}
                          />
                        </InputWrapper>
                        <Suspense>
                          <ProviderModelSettings provider={AIProvider.OpenAI} />
                        </Suspense>
                      </Column>
                    </SettingsSection>
                  )}
                  {matches('anthropic api key model default') && (
                    <SettingsSection
                      label='Anthropic'
                      childSearchKeywords='anthropic api key model default'
                    >
                      <Column gap='0.5rem'>
                        <label htmlFor='anthropic-api-key'>
                          Anthropic API key
                        </label>
                        <InputWrapper>
                          <InputStyled
                            id='anthropic-api-key'
                            type='password'
                            autoComplete='new-password'
                            value={anthropicApiKey || ''}
                            onChange={event =>
                              setAnthropicApiKey(
                                event.target.value || undefined,
                              )
                            }
                          />
                        </InputWrapper>
                        <Subtle>
                          This key stays in this browser's local storage.
                          Requests are sent directly over HTTPS from this
                          browser to Anthropic and require provider access and
                          browser CORS permission.
                        </Subtle>
                        <label htmlFor='anthropic-model-id'>
                          Anthropic model ID
                        </label>
                        <InputWrapper>
                          <InputStyled
                            id='anthropic-model-id'
                            value={anthropicModelId}
                            onChange={event => {
                              const id = event.target.value;
                              setAnthropicModelId(id);

                              if (
                                defaultChatModel.provider ===
                                AIProvider.Anthropic
                              ) {
                                setDefaultChatModel({
                                  id,
                                  provider: AIProvider.Anthropic,
                                });
                              }
                            }}
                          />
                        </InputWrapper>
                        <Suspense>
                          <ProviderModelSettings
                            provider={AIProvider.Anthropic}
                          />
                        </Suspense>
                      </Column>
                    </SettingsSection>
                  )}
                  {matches(
                    'openai-compatible proxy endpoint api key model',
                  ) && (
                    <SettingsSection
                      label='OpenAI-compatible proxy'
                      childSearchKeywords='openai-compatible proxy endpoint api key model'
                    >
                      <Column gap='0.5rem'>
                        <label htmlFor='openai-compatible-base-url'>
                          Base URL
                        </label>
                        <InputWrapper>
                          <InputStyled
                            id='openai-compatible-base-url'
                            type='url'
                            value={openAICompatibleBaseURL || ''}
                            placeholder='https://proxy.example/v1'
                            onChange={event =>
                              setOpenAICompatibleBaseURL(
                                event.target.value || undefined,
                              )
                            }
                          />
                        </InputWrapper>
                        <label htmlFor='openai-compatible-api-key'>
                          API key (optional)
                        </label>
                        <InputWrapper>
                          <InputStyled
                            id='openai-compatible-api-key'
                            type='password'
                            autoComplete='new-password'
                            value={openAICompatibleApiKey || ''}
                            onChange={event =>
                              setOpenAICompatibleApiKey(
                                event.target.value || undefined,
                              )
                            }
                          />
                        </InputWrapper>
                        <label htmlFor='openai-compatible-model-id'>
                          Model ID
                        </label>
                        <InputWrapper>
                          <InputStyled
                            id='openai-compatible-model-id'
                            value={openAICompatibleModelId}
                            onChange={event => {
                              const id = event.target.value;
                              setOpenAICompatibleModelId(id);

                              if (
                                defaultChatModel.provider ===
                                AIProvider.OpenAICompatible
                              ) {
                                setDefaultChatModel({
                                  id,
                                  provider: AIProvider.OpenAICompatible,
                                });
                              }
                            }}
                          />
                        </InputWrapper>
                        <Subtle>
                          Keys are stored in this browser's local storage.
                          Requests go directly from this browser to the
                          configured provider or proxy, which must be reachable
                          from the browser and allow CORS. Use HTTPS except for
                          local development endpoints. No endpoint checks are
                          made until you send a request.
                        </Subtle>
                        <Suspense>
                          <ProviderModelSettings
                            provider={AIProvider.OpenAICompatible}
                          />
                        </Suspense>
                      </Column>
                    </SettingsSection>
                  )}
                  {matches(
                    'openrouter api key credits model default privacy retention zdr',
                  ) && (
                    <SettingsSection
                      label='OpenRouter'
                      childSearchKeywords='openrouter api key credits model default privacy retention zdr'
                    >
                      <Column gap='0.5rem'>
                        {matches('openrouter api key credits') && (
                          <Column gap='0.5rem'>
                            <ConditionalSettings
                              fullWidth
                              gap='0.5rem'
                              enabled={true}
                            >
                              <label htmlFor='openrouter-api-key'>
                                OpenRouter API Key
                              </label>
                              <Row center>
                                {!openRouterApiKey && (
                                  <>
                                    <OpenRouterLoginButton />
                                    or
                                  </>
                                )}
                                <InputWrapper>
                                  <InputStyled
                                    id='openrouter-api-key'
                                    type='password'
                                    value={openRouterApiKey || ''}
                                    onChange={e =>
                                      handleSetOpenRouterKey(
                                        e.target.value || undefined,
                                      )
                                    }
                                    placeholder='Enter your OpenRouter API key'
                                  />
                                </InputWrapper>
                              </Row>
                              {creditUsage && (
                                <Subtle>
                                  Credits used: {intl.format(creditUsage.used)}{' '}
                                  / {intl.format(creditUsage.total)}
                                </Subtle>
                              )}
                              {!openRouterApiKey && (
                                <Subtle>
                                  OpenRouter provides a unified API that gives
                                  you access to hundreds of AI models from all
                                  major vendors, while automatically handling
                                  fallbacks and selecting the most
                                  cost-effective options.
                                </Subtle>
                              )}
                            </ConditionalSettings>
                          </Column>
                        )}
                        {matches(
                          'openrouter model default privacy retention zdr',
                        ) && (
                          <Suspense>
                            <ProviderModelSettings
                              provider={AIProvider.OpenRouter}
                            />
                          </Suspense>
                        )}
                      </Column>
                    </SettingsSection>
                  )}

                  {matches('ollama api url local server model default') && (
                    <SettingsSection
                      label='Ollama'
                      childSearchKeywords='ollama api url local server model default'
                    >
                      <Column gap='0.5rem'>
                        {matches('ollama api url local server') && (
                          <Column gap='0.5rem'>
                            {!ollamaUrl && <LocalOllamaDiscovery />}
                            <Subtle>
                              Host your own AI models locally using{' '}
                              <a
                                href='https://ollama.com/'
                                target='_blank'
                                rel='noreferrer'
                              >
                                Ollama
                              </a>
                            </Subtle>
                            <ConditionalSettings
                              fullWidth
                              gap='0.5rem'
                              enabled={true}
                            >
                              <Row center gap='1ch'>
                                {ollamaUrl &&
                                  (isOllamaUrlValid ? (
                                    <FaCheck
                                      title='Server found'
                                      color={theme.colors.main}
                                    />
                                  ) : (
                                    <FaTriangleExclamation
                                      title='Server not responding'
                                      color={theme.colors.warning}
                                    />
                                  ))}
                                <label htmlFor='ollama-url'>
                                  Ollama API Url
                                </label>
                              </Row>
                              <InputWrapper>
                                <InputStyled
                                  id='ollama-url'
                                  value={ollamaUrl || ''}
                                  onChange={e =>
                                    setOllamaUrl(e.target.value || undefined)
                                  }
                                  type='url'
                                  placeholder='http://localhost:11434'
                                />
                              </InputWrapper>
                            </ConditionalSettings>
                          </Column>
                        )}
                        {matches('ollama model default') && (
                          <Suspense>
                            <ProviderModelSettings
                              provider={AIProvider.Ollama}
                            />
                          </Suspense>
                        )}
                      </Column>
                    </SettingsSection>
                  )}

                  <SpeechSettings />
                  <Suspense fallback={<span>Loading AI settings…</span>}>
                    <AIConfigurationSections />
                  </Suspense>
                </SubGroup>
              </ConditionalSettings>
            </Column>
          </SettingsSearchProvider>
        </SettingsContent>
      </Details>
    </SettingsSectionWrapper>
  );
};

const ConditionalSettings = styled(Column)<{ enabled: boolean }>`
  opacity: ${p => (p.enabled ? 1 : 0.3)};
  pointer-events: ${p => (p.enabled ? 'auto' : 'none')};
  touch-action: ${p => (p.enabled ? 'auto' : 'none')};
  ${transition('opacity')}
`;

const SubGroup = styled.div`
  margin-top: 0.25rem;
  display: flex;
  flex-direction: column;
  gap: 0;

  button[aria-label='collapse'],
  button[aria-label='expand'] {
    height: 1.5em;
    background: transparent !important;
    box-shadow: none !important;
  }
`;

const Subtle = styled.p`
  font-size: 0.8rem;
  margin: 0;
  color: ${p => p.theme.colors.textLight};
`;

export default AISettings;
