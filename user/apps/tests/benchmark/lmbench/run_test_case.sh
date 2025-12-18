#! /bin/bash

main(){
    bash ./init.sh
    bash "$1"
    bash ./clean_up.sh
}

main "$@"
