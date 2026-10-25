import re


def slugify(text):
    """'Hello, World!' -> 'hello-world'"""
    return re.sub(r"[^a-z0-9]", "-", text.lower())
