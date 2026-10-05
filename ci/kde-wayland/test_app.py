#!/usr/bin/env python3
"""Target app for the KDE Wayland Computer Use test (see run.sh).

Two windows; every result the test checks is shown as label text, which
the test reads back through AT-SPI.
"""
import sys

from PySide6.QtGui import QKeySequence, QShortcut
from PySide6.QtWidgets import (
    QApplication,
    QLabel,
    QLineEdit,
    QListWidget,
    QPushButton,
    QVBoxLayout,
    QWidget,
)


class Window(QWidget):
    def __init__(self, title, name, status):
        super().__init__()
        self.setWindowTitle(title)
        self.name = name
        self.status = status

    def changeEvent(self, event):
        if self.isActiveWindow() and self.status is not None:
            self.status.setText(f"active: {self.name}")
        super().changeEvent(event)


def main():
    app = QApplication(sys.argv)
    app.setApplicationName("todex-kde-test")
    app.setDesktopFileName("todex-kde-test")

    active = QLabel("active: none")
    main_window = Window("TodeX KDE Test", "main", active)
    layout = QVBoxLayout(main_window)

    clicks = QLabel("clicks: 0")
    button = QPushButton("Press me")
    count = [0]

    def pressed():
        count[0] += 1
        clicks.setText(f"clicks: {count[0]}")

    button.clicked.connect(pressed)

    entry = QLineEdit()
    entry.setAccessibleName("Entry")
    echo = QLabel("typed: ")
    entry.textChanged.connect(lambda text: echo.setText(f"typed: {text}"))

    chord = QLabel("chord: none")
    shortcut = QShortcut(QKeySequence("Ctrl+Shift+K"), main_window)
    shortcut.activated.connect(lambda: chord.setText("chord: ok"))

    items = QListWidget()
    items.setAccessibleName("Items")
    items.addItems([f"Item {index}" for index in range(300)])
    scrolled = QLabel("scrolled: 0")
    items.verticalScrollBar().valueChanged.connect(
        lambda value: scrolled.setText(f"scrolled: {value}")
    )

    for widget in (button, clicks, entry, echo, chord, active, items, scrolled):
        layout.addWidget(widget)
    main_window.resize(600, 700)
    main_window.show()

    second = Window("TodeX KDE Second", "second", active)
    QVBoxLayout(second).addWidget(QLabel("second window"))
    second.resize(300, 200)
    second.show()
    main_window.activateWindow()

    sys.exit(app.exec())


if __name__ == "__main__":
    main()
