cmake_minimum_required(VERSION 3.28)
project(faxe_native LANGUAGES C CXX)
add_subdirectory("${PJ_SOURCE}" pj EXCLUDE_FROM_ALL)
add_custom_target(faxe-libraries DEPENDS pjsip-ua pjmedia-codec)
foreach(component pjlib pjlib-util pjnath pjmedia pjmedia-codec pjsip pjsip-ua)
  file(GENERATE OUTPUT "${CMAKE_BINARY_DIR}/${component}.includes"
    CONTENT "$<JOIN:$<TARGET_PROPERTY:${component},INCLUDE_DIRECTORIES>,\n>"
    TARGET ${component})
  file(GENERATE OUTPUT "${CMAKE_BINARY_DIR}/${component}.defines"
    CONTENT "$<JOIN:$<TARGET_PROPERTY:${component},COMPILE_DEFINITIONS>,\n>"
    TARGET ${component})
endforeach()
