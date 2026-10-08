import unittest
from proviz_elekto.client import ProvizElekto, _classify_error, _error_retry_after_ms

class SpeedPolicyTests(unittest.TestCase):
    def client(self):
        client = object.__new__(ProvizElekto)
        client._timeout = 10
        self.requests = []
        def post(path, payload):
            self.requests.append((path, payload))
            return {"model_id": "model", "brand_slug": "brand", "model_slug": "model",
                    "max_context_tokens": 32000, "supports_function_calling": True,
                    "supports_json_mode": True}
        client._post = post
        return client

    def test_quota_error_shapes_and_retry_after(self):
        error = RuntimeError("INSUFFICIENT QUOTA")
        error.headers = {"Retry-After": "0.1"}
        self.assertEqual(_classify_error(error), ("rate_limit", "rpm"))
        self.assertEqual(_error_retry_after_ms(error), 101)
        error.headers = {"Retry-After": "nan"}
        self.assertIsNone(_error_retry_after_ms(error))

    def test_select_and_complete_forward_policy_and_pin_wait(self):
        client = self.client()
        client.select("chat", 8000, max_latency_ms=5000, max_latency_ratio=2,
                      estimated_output_tokens=1000, pin_model="brand/model", pin_wait=True)
        payload = self.requests[-1][1]
        self.assertEqual(payload["estimated_output_tokens"], 1000)
        self.assertEqual(payload["max_latency_ratio"], 2)
        self.assertTrue(payload["pin_wait"])
        client.complete("chat", [], max_tokens=500, max_latency_ms=5000,
                        pin_model="brand/model", pin_wait=True, max_wait_ms=100)
        self.assertEqual(self.requests[-1][1]["max_tokens"], 500)
        self.assertEqual(self.requests[-1][1]["max_latency_ms"], 5000)

    def test_quota_report_echoes_reservation_and_account_cooldown(self):
        client = self.client()
        client.report_rate_limit("model", brand_key_id="key", estimated_tokens=8000,
                                 retry_after_ms=100, quota_scope_brand=True)
        payload = self.requests[-1][1]
        self.assertEqual(payload["estimated_tokens"], 8000)
        self.assertEqual(payload["brand_key_id"], "key")
        self.assertTrue(payload["quota_scope_brand"])
        self.assertNotIn("response_time_ms", payload)

if __name__ == "__main__":
    unittest.main()
