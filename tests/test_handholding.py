import unittest

from donut import handholding


class HandholdingTests(unittest.TestCase):
    def test_each_level_tells_the_planner_how_big_and_exact_tickets_are(self):
        self.assertEqual(handholding.level({}), "medium")
        self.assertEqual(handholding.level(None), "medium")
        self.assertEqual(handholding.level({"handholding": "bogus"}), "medium")  # Unknown: the default.
        low = handholding.instruction({"handholding": "low"})
        medium = handholding.instruction({})
        high = handholding.instruction({"handholding": "high"})
        self.assertIn("Ticket scope can be larger", low)
        self.assertIn("leave the implementation approach to the worker", low)
        self.assertIn("one behavior per ticket", medium)
        self.assertIn("weak model", high)
        self.assertIn("exact code to insert or replace", high)
        for text in (low, medium, high):
            self.assertIn("overrides the planner skill's ticket-size guideline", text)


if __name__ == "__main__":
    unittest.main()
