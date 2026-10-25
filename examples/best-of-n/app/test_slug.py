import unittest

from slug import slugify


class SlugifyTest(unittest.TestCase):
    def test_punctuation(self):
        self.assertEqual(slugify("Hello, World!"), "hello-world")

    def test_spaces(self):
        self.assertEqual(slugify("  many   spaces "), "many-spaces")

    def test_symbols(self):
        self.assertEqual(slugify("C++ & Rust"), "c-rust")

    def test_already_a_slug(self):
        self.assertEqual(slugify("already-a-slug"), "already-a-slug")


if __name__ == "__main__":
    unittest.main()
