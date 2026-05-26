"""pytest fixtures for the Roost E2E suite.

Parameterized by target UI (`--roost-target mac|gtk`, default `$ROOST_TARGET`
or `gtk`). A session fixture ensures the UI is running (launching it if
needed, and quitting only what it launched). Each test gets a fresh
`roost` client and a throwaway `project` that's cascade-cleaned after.
"""

from __future__ import annotations

import os
import uuid

import pytest
import ui
from client import Roost


def pytest_addoption(parser):
    parser.addoption(
        "--roost-target", action="store", default=None, choices=list(ui.TARGETS),
        help="which UI to drive (mac|gtk); default $ROOST_TARGET or gtk",
    )


@pytest.fixture(scope="session")
def target(pytestconfig) -> str:
    return (
        pytestconfig.getoption("--roost-target")
        or os.environ.get("ROOST_TARGET")
        or "gtk"
    )


@pytest.fixture(scope="session", autouse=True)
def _ui_session(target):
    started_here = not ui.is_alive(target)
    ui.launch(target)
    yield
    if started_here:  # leave a UI the developer already had running
        ui.quit(target)


@pytest.fixture
def roost(target):
    client = Roost(ui.socket_path(target))
    try:
        yield client
    finally:
        client.close()


@pytest.fixture
def project(roost):
    """A throwaway project for one test; cascade-cleaned afterward."""
    pid = roost.create_project(name=f"pytest-{uuid.uuid4().hex[:8]}", cwd="/tmp")
    try:
        yield pid
    finally:
        try:
            roost.delete_project(pid)
        except Exception:
            pass  # a test may have already cascade-closed it
