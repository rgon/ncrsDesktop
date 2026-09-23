# SPDX-License-Identifier: GPL-3.0-or-later
#
# Dolphin / KIO overlay-icon plugin for ncrs.
#   cmake -B build -DCMAKE_INSTALL_PREFIX=/usr                       # Qt6 / KF6
#   cmake -B build -DCMAKE_INSTALL_PREFIX=/usr -DQT_MAJOR_VERSION=5  # Qt5 / KF5
cmake_minimum_required(VERSION 3.16)
project(ncrs-dolphin LANGUAGES CXX)

set(QT_MAJOR_VERSION 6 CACHE STRING "Qt and KDE Frameworks major version to build against (5 or 6)")
if(QT_MAJOR_VERSION EQUAL 6)
    set(QT_MIN_VERSION 6.4.0)
    set(KF_MIN_VERSION 6.0.0)
elseif(QT_MAJOR_VERSION EQUAL 5)
    set(QT_MIN_VERSION 5.15.2)
    set(KF_MIN_VERSION 5.90.0)
else()
    message(FATAL_ERROR "QT_MAJOR_VERSION must be 5 or 6, got '${QT_MAJOR_VERSION}'")
endif()
set(KF KF${QT_MAJOR_VERSION})
set(QT Qt${QT_MAJOR_VERSION})
# KOverlayIconPlugin moved from KIOWidgets (KF5) to KIOCore (KF6).
if(QT_MAJOR_VERSION EQUAL 6)
    set(KIO_OVERLAY_LIB KF6::KIOCore)
else()
    set(KIO_OVERLAY_LIB KF5::KIOWidgets)
endif()

find_package(ECM ${KF_MIN_VERSION} REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

# Plugins must land in Qt's own plugin dir (e.g. /usr/lib/x86_64-linux-gnu/qt6/plugins),
# which is where Dolphin looks and where ncrs detects the adapter.
set(KDE_INSTALL_USE_QT_SYS_PATHS ON CACHE BOOL "Install plugins into Qt's plugin directory")

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)
include(FeatureSummary)

find_package(${QT} ${QT_MIN_VERSION} REQUIRED COMPONENTS Core Network)
find_package(${KF} ${KF_MIN_VERSION} REQUIRED COMPONENTS CoreAddons KIO)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Client + status table, shared by the plugin and the tests.
add_library(ncrsclient_static STATIC src/ncrsclient.cpp src/ncrsemblems.cpp)
set_target_properties(ncrsclient_static PROPERTIES POSITION_INDEPENDENT_CODE ON)
target_include_directories(ncrsclient_static PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/src)
target_link_libraries(ncrsclient_static PUBLIC ${QT}::Core ${QT}::Network)

# Installs as <qtplugins>/kf<N>/overlayicon/ncrsoverlayplugin.so; the file
# name is what ncrs_core/src/desktop/browser.rs checks for.
kcoreaddons_add_plugin(ncrsoverlayplugin
    SOURCES src/ncrsoverlayplugin.cpp
    INSTALL_NAMESPACE "kf${QT_MAJOR_VERSION}/overlayicon")
target_link_libraries(ncrsoverlayplugin PRIVATE ncrsclient_static ${KF}::CoreAddons ${KIO_OVERLAY_LIB})

# ServiceMenu: kio/servicemenus is read by KF6 and by KF5 >= 5.85, so the KF5
# and KF6 builds install the same file to the same path and one package can
# carry both without a duplicate menu entry.
install(FILES servicemenu/ncrs.desktop DESTINATION ${KDE_INSTALL_DATADIR}/kio/servicemenus)

if(BUILD_TESTING)
    add_subdirectory(tests)
endif()

feature_summary(WHAT ALL INCLUDE_QUIET_PACKAGES FATAL_ON_MISSING_REQUIRED_PACKAGES)
