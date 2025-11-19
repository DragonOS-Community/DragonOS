package main

import (
	"fmt"
	"math/rand"
	"sync"
	"time"
)

func main() {
	fmt.Println("🚀 Go ASCII Art Demo with Goroutines 🚀")
	fmt.Println("==========================================")

	var wg sync.WaitGroup

	// // Launch multiple goroutines with different ASCII art
	wg.Add(4)

	// Goroutine 1: Dancing person
	go func() {
		defer wg.Done()
		showDancingPerson()
	}()

	// Goroutine 2: Rotating cube
	go func() {
		defer wg.Done()
		showRotatingCube()
	}()

	// Goroutine 3: Fire animation
	go func() {
		defer wg.Done()
		showFireAnimation()
	}()

	// Goroutine 4: Space rocket
	go func() {
		defer wg.Done()
		showRocket()
	}()

	// // Wait for all goroutines to finish
	wg.Wait()

	fmt.Println("\n✨ All animations completed! ✨")
}

func showDancingPerson() {
	frames := []string{
		`
     o
    /|\
    / \     ♪`,
		`
     o
    /|\
    / \     ♫`,
		`
    \o/
     |
    / \    ♪`,
		`
     o
    /|\
    / \     ♬`,
	}

	for i := 0; i < 8; i++ {
		fmt.Printf("\nDancing Person:\n%s\n", frames[i%len(frames)])
		time.Sleep(300 * time.Millisecond)
	}
}

func showRotatingCube() {
	frames := []string{
		`
    ╔═════╗
   ║░░░░░║
   ║░░░░░║
   ╚═════╝`,
		`
    ┌─────┐
   ╱░░░░░╲
  ╱░░░░░░╲
  ╲░░░░░░╱
   ╲░░░░░╱
    └─────┘`,
		`
      ╔═╗
     ╔═╝╚═╗
     ║░░░░║
     ╚═╗╔═╝
      ╚═╝`,
		`
    ┌─────┐
   ╲░░░░░╱
    ╲░░░░╱
     ╲░░░╱
      ╲░░╱
       ──`,
	}

	for i := 0; i < 10; i++ {
		fmt.Printf("\nRotating Cube:\n%s\n", frames[i%len(frames)])
		time.Sleep(250 * time.Millisecond)
	}
}

func showFireAnimation() {
	frames := []string{
		`
       🔥
      🔥🔥
     🔥🔥🔥
    🔥🔥🔥🔥
   🔥🔥🔥🔥🔥`,
		`
         ^^^
        ^^^^^
       ^^^^^^^
      ^^^^^^^^^
     ^^^^^^^^^^^`,
		`
        /\_/\
       ( o.o )
        > ^ <  `,
		`
     ╱╲╱╲╱╲
    ╱╲╱╲╱╲╱╲
   ╱╲╱╲╱╲╱╲╱╲`,
	}

	for i := 0; i < 12; i++ {
		fmt.Printf("\nFire Animation:\n%s\n", frames[i%len(frames)])
		time.Sleep(200 * time.Millisecond)
	}
}

func showRocket() {
	rocket := []string{
		`
     ^
    / \\
   |   |
   |   |
   |   |
  /|   |\\
 / |___| \\
   |||||
   |||||`,
	}

	for i := 0; i < 15; i++ {
		// Clear screen and show rocket at different positions
		fmt.Printf("\nRocket Launch:\n")
		for j := 0; j < i%5; j++ {
			fmt.Println()
		}
		fmt.Print(rocket[0])

		// Add stars
		stars := rand.Intn(3) + 1
		for k := 0; k < stars; k++ {
			fmt.Printf(" %s", "✨")
		}

		time.Sleep(400 * time.Millisecond)
	}
}

// No need for explicit seeding in Go 1.21+ - global rand is automatically seeded
