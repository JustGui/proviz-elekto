import os
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from proviz_elekto.client import _litellm_target, _provider_api_key, ProvizElekto


class CloudflareTests(unittest.TestCase):
    def test_key_alias_and_precedence(self):
        for name in ('CLOUDFLARE_API_KEY', 'CLOUDFARE_API_KEY'):
            with patch.dict(os.environ, {'CLOUDFARE_API_KEY': 'legacy'}, clear=True):
                self.assertEqual(_provider_api_key(name), 'legacy')
                os.environ['CLOUDFLARE_API_KEY'] = 'correct'
                self.assertEqual(_provider_api_key(name), 'correct')
        with patch.dict(os.environ, {}, clear=True):
            self.assertIsNone(_provider_api_key('CLOUDFLARE_API_KEY'))

    def test_chat_split_flow_target(self):
        candidate = SimpleNamespace(brand_slug='cloudflare', model_slug='@cf/example/chat',
                                    base_url='https://example.test/ai/v1')
        self.assertEqual(_litellm_target(candidate), ('openai/@cf/example/chat',
                         {'api_base': 'https://example.test/ai/v1'}))

    def test_complete_preserves_decision_confidence(self):
        client = object.__new__(ProvizElekto)
        client._timeout = 10
        client._post = lambda *a, **kw: dict(text='{"decision":"duplicate"}',
            model='typesafe/jev', brand='cloudflare', prompt_tokens=20, completion_tokens=3,
            cost_usd=0.00000084, decision_probabilities={'decision': {'confidence': 0.97}})
        result = client.complete('decision', [], group_name='jev', categories=['decision'],
                                 require_no_training=True)
        self.assertEqual(result.brand, 'cloudflare')
        self.assertEqual(result.decision_probabilities['decision']['confidence'], 0.97)


if __name__ == '__main__':
    unittest.main()
