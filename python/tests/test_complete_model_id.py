import unittest

from proviz_elekto.client import ProvizElekto


def _client(response):
    client = object.__new__(ProvizElekto)
    client._timeout = 10
    client._post = lambda *a, **kw: response
    return client


class CompleteModelIdTests(unittest.TestCase):
    def test_model_id_returned_for_retry_exclusion(self):
        mid = '7d3c1f2e-0000-4000-8000-000000000001'
        result = _client(dict(text='x', model='m', brand='b', prompt_tokens=1, completion_tokens=1,
                              model_id=mid)).complete('chat', [])
        self.assertEqual(result.model_id, mid)

    def test_older_server_without_model_id(self):
        result = _client(dict(text='x', model='m', brand='b', prompt_tokens=1,
                              completion_tokens=1)).complete('chat', [])
        self.assertIsNone(result.model_id)


if __name__ == '__main__':
    unittest.main()
