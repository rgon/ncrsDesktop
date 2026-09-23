# SPDX-License-Identifier: GPL-3.0-or-later
include(ECMAddTests)

find_package(${QT} ${QT_MIN_VERSION} REQUIRED COMPONENTS Test)

add_library(fakencrsserver STATIC fakencrsserver.cpp)
target_link_libraries(fakencrsserver PUBLIC ${QT}::Core ${QT}::Network)

ecm_add_test(ncrsclienttest.cpp
    TEST_NAME ncrsclienttest
    LINK_LIBRARIES ncrsclient_static fakencrsserver ${QT}::Test)

# The plugin class is compiled straight into the test so it can be handed a
# client wired to the fake server.
ecm_add_test(ncrsoverlayplugintest.cpp ${PROJECT_SOURCE_DIR}/src/ncrsoverlayplugin.cpp
    TEST_NAME ncrsoverlayplugintest
    LINK_LIBRARIES ncrsclient_static fakencrsserver ${QT}::Test ${KIO_OVERLAY_LIB})

ecm_add_test(ncrsemblemstest.cpp
    TEST_NAME ncrsemblemstest
    LINK_LIBRARIES ncrsclient_static ${QT}::Test)
# The shared vocabulary every adapter's tests read (Rust, Python and C++).
target_compile_definitions(ncrsemblemstest PRIVATE
    NCRS_STATUS_VOCABULARY="${PROJECT_SOURCE_DIR}/../status-vocabulary.txt"
    NCRS_BREEZE_ICON_DIR="/usr/share/icons/breeze")

ecm_add_test(ncrspluginloadtest.cpp
    TEST_NAME ncrspluginloadtest
    LINK_LIBRARIES ${QT}::Test ${KF}::CoreAddons ${KIO_OVERLAY_LIB})
target_compile_definitions(ncrspluginloadtest PRIVATE NCRS_PLUGIN_FILE="$<TARGET_FILE:ncrsoverlayplugin>")
add_dependencies(ncrspluginloadtest ncrsoverlayplugin)
