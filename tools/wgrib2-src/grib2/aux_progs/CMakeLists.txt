cmake_minimum_required(VERSION 3.16.3)
project (aux_progs)

add_executable(gmerge gmerge.c uint8.c)
add_executable(smallest_grib2 smallest_grib2.c uint8.c)
add_executable(smallest_4 smallest_4.c uint8.c)

