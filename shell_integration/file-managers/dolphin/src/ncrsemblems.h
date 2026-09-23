// SPDX-License-Identifier: GPL-3.0-or-later
//
// Sync status → Breeze emblem. The single source of truth for what Dolphin
// draws; tests/ncrsemblemstest.cpp checks it covers status-vocabulary.txt.
#pragma once

#include <QStringList>

struct NcrsEntry;

struct NcrsStatusEmblem {
    const char *status; // a word from status-vocabulary.txt
    const char *emblem; // Breeze icon name, or nullptr for "no emblem"
};

// Every status word the daemon can send, including those that draw nothing.
extern const NcrsStatusEmblem kNcrsStatusEmblems[];
extern const int kNcrsStatusEmblemCount;
// Added after the status emblem when the item carries any sharing.
extern const char kNcrsSharedEmblem[];

// Emblem for one status word; empty for "none" and for words not in the table.
QString ncrsEmblemForStatus(const QString &status);
bool ncrsIsKnownStatus(const QString &status);
// Overlays for an entry, in KIO's corner order (first = bottom right).
QStringList ncrsOverlays(const NcrsEntry &entry);
